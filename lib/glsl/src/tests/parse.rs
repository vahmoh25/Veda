//! The lexer and the parser.

use std::format;
use std::string::{String, ToString};
use std::vec::Vec;

use crate::ast::*;
use crate::diag::Diagnostics;
use crate::intern::Interner;
use crate::pp::{self, ExtSet};
use crate::{lex, parse};

fn parse_src(src: &str) -> (TranslationUnit, Interner, String) {
    let mut interner = Interner::new();
    let mut diags = Diagnostics::new();
    let pre = pp::preprocess(&[src], ExtSet::all(), &mut interner, &mut diags);
    let toks = lex::tokens(&pre.tokens, pre.version, pre.enabled, &interner, &mut diags);
    let unit = parse::parse(&toks, &interner, &mut diags);
    (unit, interner, diags.log())
}

fn parses(src: &str) -> (TranslationUnit, Interner) {
    let (unit, interner, log) = parse_src(src);
    assert!(log.is_empty(), "unexpected messages for:\n{src}\n{log}");
    (unit, interner)
}

fn fails(src: &str, needle: &str) {
    let (_, _, log) = parse_src(src);
    assert!(log.contains("ERROR"), "expected an error for:\n{src}");
    assert!(log.contains(needle), "log lacks {needle:?}:\n{log}");
}

/// An expression as an S-expression, for checking the tree's shape.
fn sexpr(e: &Expr, i: &Interner) -> String {
    match &e.kind {
        ExprKind::Ident(s) => i.get(*s).to_string(),
        ExprKind::Int(v) => format!("{}", *v as i32),
        ExprKind::Uint(v) => format!("{v}u"),
        ExprKind::Float(v) => format!("{v:?}"),
        ExprKind::Bool(b) => format!("{b}"),
        ExprKind::Unary(op, a) => format!("({op:?} {})", sexpr(a, i)),
        ExprKind::Binary(op, a, b) => format!("({} {} {})", op.spelling(), sexpr(a, i), sexpr(b, i)),
        ExprKind::Assign(op, a, b) => {
            let o = op.map_or("=".to_string(), |o| format!("{}=", o.spelling()));
            format!("({o} {} {})", sexpr(a, i), sexpr(b, i))
        }
        ExprKind::Ternary(c, a, b) => format!("(? {} {} {})", sexpr(c, i), sexpr(a, i), sexpr(b, i)),
        ExprKind::Comma(a, b) => format!("(, {} {})", sexpr(a, i), sexpr(b, i)),
        ExprKind::Index(a, b) => format!("([] {} {})", sexpr(a, i), sexpr(b, i)),
        ExprKind::Field(a, f) => format!("(. {} {})", sexpr(a, i), i.get(*f)),
        ExprKind::Length(a) => format!("(length {})", sexpr(a, i)),
        ExprKind::Call(callee, args) => {
            let name = match callee {
                Callee::Name(s) => i.get(*s).to_string(),
                Callee::NamedArray(s, Some(n)) => format!("{}[{}]", i.get(*s), sexpr(n, i)),
                Callee::NamedArray(s, None) => format!("{}[]", i.get(*s)),
                Callee::Type(t) => match (&t.name, &t.array) {
                    (TypeName::Basic(b), None) => b.name(),
                    (TypeName::Basic(b), Some(ArraySize::Unsized)) => format!("{}[]", b.name()),
                    (TypeName::Basic(b), Some(ArraySize::Sized(n))) => format!("{}[{}]", b.name(), sexpr(n, i)),
                    _ => "?".into(),
                },
            };
            let args: Vec<String> = args.iter().map(|a| sexpr(a, i)).collect();
            format!("(call {name} {})", args.join(" "))
        }
    }
}

/// The S-expression of `expr` parsed as a statement in a function.
fn expr(e: &str) -> String {
    let src = format!("void f() {{ {e}; }}");
    let (unit, i) = parses(&src);
    let External::Function(f) = &unit.items[0] else { panic!("no function") };
    match &f.body[0].kind {
        StmtKind::Expr(e) => sexpr(e, &i),
        other => panic!("not an expression statement: {other:?}"),
    }
}

#[test]
fn operators_bind_by_precedence() {
    assert_eq!(expr("a + b * c"), "(+ a (* b c))");
    assert_eq!(expr("a * b + c"), "(+ (* a b) c)");
    assert_eq!(expr("a - b - c"), "(- (- a b) c)");
    assert_eq!(expr("a || b ^^ c && d"), "(|| a (^^ b (&& c d)))");
    assert_eq!(expr("a | b ^ c & d"), "(| a (^ b (& c d)))");
    assert_eq!(expr("a == b < c << d"), "(== a (< b (<< c d)))");
    assert_eq!(expr("a = b = c"), "(= a (= b c))");
    assert_eq!(expr("a += b ? c : d"), "(+= a (? b c d))");
    assert_eq!(expr("a ? b : c ? d : e"), "(? a b (? c d e))");
    assert_eq!(expr("a, b = c"), "(, a (= b c))");
    assert_eq!(expr("-a.x[1]++"), "(Minus (PostInc ([] (. a x) 1)))");
    assert_eq!(expr("!~++a"), "(Not (BitNot (PreInc a)))");
}

#[test]
fn calls_constructors_and_methods() {
    assert_eq!(expr("f(a, g(b))"), "(call f a (call g b))");
    assert_eq!(expr("vec4(1.0).xyz"), "(. (call vec4 1.0) xyz)");
    assert_eq!(expr("f(void)"), "(call f )");
    assert_eq!(expr("a.length()"), "(length a)");
    assert_eq!(expr("S(1.0, 2)"), "(call S 1.0 2)");
}

#[test]
fn array_constructors() {
    let (unit, i) = parses("#version 300 es\nvoid f() { float[3](1.0, 2.0, 3.0); S[2](a, b); float[](x); S[](y); }");
    let External::Function(f) = &unit.items[0] else { panic!() };
    let got: Vec<String> = f
        .body
        .iter()
        .map(|s| match &s.kind {
            StmtKind::Expr(e) => sexpr(e, &i),
            _ => "?".into(),
        })
        .collect();
    assert_eq!(got, ["(call float[3] 1.0 2.0 3.0)", "(call S[2] a b)", "(call float[] x)", "(call S[] y)"]);
}

#[test]
fn literals_follow_the_versions_rules() {
    assert_eq!(expr("0x80000000"), "-2147483648");
    assert_eq!(expr("3000000000"), "-1294967296");
    assert_eq!(expr("017"), "15");
    assert_eq!(expr("1.5e2"), "150.0");
    assert_eq!(expr(".5"), "0.5");
    assert_eq!(expr("2."), "2.0");
    fails("void f() { 5000000000; }", "needs more than 32 bits");
    fails("void f() { 08; }", "invalid integer constant");
    fails("void f() { 1u; }", "unsigned integers need GLSL ES 3.00");
    fails("void f() { 1.0f; }", "floating-point suffixes need GLSL ES 3.00");
    let (unit, i) = parses("#version 300 es\nvoid f() { 1u; 2.5f; 0xFFFFFFFFu; }");
    let External::Function(f) = &unit.items[0] else { panic!() };
    let got: Vec<String> = f
        .body
        .iter()
        .map(|s| match &s.kind {
            StmtKind::Expr(e) => sexpr(e, &i),
            _ => "?".into(),
        })
        .collect();
    assert_eq!(got, ["1u", "2.5", "4294967295u"]);
    fails("#version 300 es\nvoid f() { 1.0.0; }", "invalid floating-point constant");
}

#[test]
fn keywords_depend_on_the_version() {
    // `uint` and `layout` are just names in GLSL ES 1.00...
    parses("float uint; float layout;");
    // ... and keywords in 3.00, where `attribute` is reserved.
    fails("#version 300 es\nfloat uint;", "expected an identifier");
    fails("#version 300 es\nattribute vec4 a;", "'attribute' is a reserved word");
    fails("float switch;", "'switch' is a reserved word");
    fails("float goto;", "'goto' is a reserved word");
    // sampler3D needs an extension in 1.00.
    fails("uniform sampler3D s;", "'sampler3D' is a reserved word");
    parses("#extension GL_OES_texture_3D : enable\nuniform sampler3D s;");
}

#[test]
fn a_complete_es2_shader_parses() {
    let src = r#"
        precision mediump float;
        attribute vec4 position;
        attribute vec2 uv;
        uniform mat4 mvp;
        uniform sampler2D tex;
        varying vec2 v_uv;
        struct Light { vec3 dir; float power; } lights[2];
        invariant gl_Position;
        float shade(in vec3 n, const Light l, out float spec);
        float shade(in vec3 n, const Light l, out float spec) {
            spec = 0.0;
            return max(dot(n, l.dir), 0.0) * l.power;
        }
        void main() {
            v_uv = uv;
            for (int i = 0; i < 2; ++i) { if (lights[i].power > 0.0) continue; else break; }
            float s;
            gl_Position = mvp * position + vec4(shade(vec3(0.0), lights[0], s));
        }
    "#;
    let (unit, _) = parses(src);
    assert_eq!(unit.items.len(), 11);
}

#[test]
fn a_complete_es3_shader_parses() {
    let src = r#"#version 300 es
        precision highp float;
        precision highp int;
        layout(std140) uniform;
        layout(location = 0) in vec3 position;
        layout(std140, row_major) uniform Camera { mat4 view; layout(column_major) mat4 proj; } cam;
        uniform Lights { vec4 color[4]; };
        flat out int id;
        smooth centroid out vec3 normal;
        out float[2] weights;
        invariant out vec4 pos;
        void main() {
            int n = 3;
            switch (n) { case 0: case 1: n++; break; default: n--; }
            uint u = 7u >> 1u;
            while (bool b = n > 0) { n--; }
            do { n++; } while (n < 2);
            float a[] = float[](1.0, 2.0);
            weights = float[2](a[0], float(a.length()));
            gl_Position = cam.proj * cam.view * vec4(position, 1.0);
        }
    "#;
    parses(src);
}

#[test]
fn declarations_and_expression_statements_are_told_apart() {
    let (unit, _) =
        parses("struct S { float x; }; void f() { S s; S[2] t; S(1.0); s.x = 1.0; vec4(1.0).x; float[2] a; }");
    let External::Function(f) = &unit.items[1] else { panic!() };
    let kinds: Vec<&str> = f
        .body
        .iter()
        .map(|s| match s.kind {
            StmtKind::Declaration(_) => "decl",
            StmtKind::Expr(_) => "expr",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["decl", "decl", "expr", "expr", "expr", "decl"]);
}

#[test]
fn syntax_errors_are_reported_and_parsing_goes_on() {
    let (_, _, log) = parse_src("void f() { a = ; b = 1; c = ); }\nvoid g() { x +; }");
    assert_eq!(log.matches("ERROR").count(), 3, "{log}");
    assert!(log.contains("0:1: syntax error: expected an expression, found ';'"), "{log}");
    assert!(log.contains("0:2:"), "{log}");
}

#[test]
fn deep_nesting_is_an_error_not_a_crash() {
    let deep = format!("void f() {{ x = {}1{}; }}", "(".repeat(5000), ")".repeat(5000));
    fails(&deep, "nests expressions or statements too deeply");
    let blocks = format!("void f() {{ {} }}", "{".repeat(3000) + &"}".repeat(3000));
    fails(&blocks, "too deeply");
    let unary = format!("void f() {{ x = {}1; }}", "-".repeat(5000));
    fails(&unary, "too deeply");
}

#[test]
fn arrays_of_arrays_are_rejected() {
    fails("#version 300 es\nfloat a[2][3];", "arrays of arrays");
}

#[test]
fn only_length_is_a_method() {
    fails("void f() { a.size(); }", "only length() is");
}

#[test]
fn garbage_does_not_panic() {
    for src in [
        "",
        ";",
        "}",
        "{",
        "(",
        "void",
        "void f(",
        "void f() {",
        "struct",
        "struct {",
        "uniform",
        "layout(",
        "layout(x=",
        "precision",
        "invariant",
        "a b c d",
        "float[",
        "f()()",
        "#version 300 es\nlayout(location=1u) in vec4 a;",
        "void f() { for(;;) }",
        "void f() { if }",
        "void f() { switch(x) { case } }",
        "x.y.z.",
        "1 2 3",
    ] {
        let _ = parse_src(src);
    }
}
