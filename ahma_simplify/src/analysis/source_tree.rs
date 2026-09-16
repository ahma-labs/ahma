use std::path::{Path, PathBuf};

use rust_code_analysis::{Callback, ParserTrait, action, get_language_for_file};
use tree_sitter::Node as TsNode;

use crate::models::Language;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Crate,
    Private,
}

#[derive(Debug, Clone)]
pub struct CallSite {
    pub callee: String,
    pub receiver: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct FunctionNode {
    pub name: String,
    pub qualifier: Option<String>,
    pub visibility: Visibility,
    pub start_line: usize,
    pub end_line: usize,
    pub body_statements: usize,
    pub calls: Vec<CallSite>,
}

#[derive(Debug, Clone)]
pub struct SourceTree {
    pub path: PathBuf,
    pub language: Language,
    pub functions: Vec<FunctionNode>,
}

struct LangSpec {
    language: Language,
    fn_kinds: &'static [&'static str],
    call_kinds: &'static [(&'static str, &'static str)],
    receiver_kinds: &'static [(&'static str, &'static str, &'static str)],
    qualifier_kinds: &'static [(&'static str, &'static str)],
}

const BLOCK_KINDS: &[&str] = &["block", "statement_block"];

const RUST_SPEC: LangSpec = LangSpec {
    language: Language::Rust,
    fn_kinds: &["function_item"],
    call_kinds: &[
        ("call_expression", "function"),
        ("macro_invocation", "macro"),
    ],
    receiver_kinds: &[("field_expression", "value", "field")],
    qualifier_kinds: &[("impl_item", "type"), ("trait_item", "name")],
};

const JS_FN_KINDS: &[&str] = &[
    "function_declaration",
    "method_definition",
    "arrow_function",
];
const JS_CALL_KINDS: &[(&str, &str)] = &[("call_expression", "function")];
const JS_RECEIVER_KINDS: &[(&str, &str, &str)] = &[("member_expression", "object", "property")];
const JS_QUALIFIER_KINDS: &[(&str, &str)] = &[("class_declaration", "name")];

const TYPESCRIPT_SPEC: LangSpec = LangSpec {
    language: Language::TypeScript,
    fn_kinds: JS_FN_KINDS,
    call_kinds: JS_CALL_KINDS,
    receiver_kinds: JS_RECEIVER_KINDS,
    qualifier_kinds: JS_QUALIFIER_KINDS,
};

const JAVASCRIPT_SPEC: LangSpec = LangSpec {
    language: Language::JavaScript,
    fn_kinds: JS_FN_KINDS,
    call_kinds: JS_CALL_KINDS,
    receiver_kinds: JS_RECEIVER_KINDS,
    qualifier_kinds: JS_QUALIFIER_KINDS,
};

const PYTHON_SPEC: LangSpec = LangSpec {
    language: Language::Python,
    fn_kinds: &["function_definition"],
    call_kinds: &[("call", "function")],
    receiver_kinds: &[("attribute", "object", "attribute")],
    qualifier_kinds: &[("class_definition", "name")],
};

const JAVA_SPEC: LangSpec = LangSpec {
    language: Language::Java,
    fn_kinds: &["method_declaration", "constructor_declaration"],
    call_kinds: &[("method_invocation", "name")],
    receiver_kinds: &[("method_invocation", "object", "name")],
    qualifier_kinds: &[
        ("class_declaration", "name"),
        ("interface_declaration", "name"),
    ],
};

fn lang_spec(language: Language) -> Option<&'static LangSpec> {
    match language {
        Language::Rust => Some(&RUST_SPEC),
        Language::TypeScript => Some(&TYPESCRIPT_SPEC),
        Language::JavaScript => Some(&JAVASCRIPT_SPEC),
        Language::Python => Some(&PYTHON_SPEC),
        Language::Java => Some(&JAVA_SPEC),
        _ => None,
    }
}

struct SourceTreeCallback;

impl Callback for SourceTreeCallback {
    type Res = Option<Vec<FunctionNode>>;
    type Cfg = Language;

    // ParserTrait is #[doc(hidden)] upstream; a fork bump needs a recompile check here.
    fn call<T: ParserTrait>(cfg: Self::Cfg, parser: &T) -> Self::Res {
        let spec = lang_spec(cfg)?;
        let code = parser.get_code();
        // `.0` is the only public way to reach the underlying tree_sitter::Node.
        let root = parser.get_root().0;
        let mut shells = Vec::new();
        collect_shells(root, code, spec, None, &mut shells);
        Some(
            shells
                .into_iter()
                .map(|shell| finalize_shell(shell, code, spec))
                .collect(),
        )
    }
}

struct Shell<'a> {
    node: TsNode<'a>,
    name: String,
    qualifier: Option<String>,
    visibility: Visibility,
}

fn base_type_name(text: &str) -> String {
    text.split(['<', ' ']).next().unwrap_or(text).to_string()
}

fn qualifier_for(
    node: TsNode,
    code: &[u8],
    spec: &LangSpec,
    current: Option<String>,
) -> Option<String> {
    for (kind, field) in spec.qualifier_kinds {
        if node.kind() == *kind
            && let Some(name_node) = node.child_by_field_name(field)
            && let Ok(text) = name_node.utf8_text(code)
        {
            return Some(base_type_name(text));
        }
    }
    current
}

fn extract_name(node: TsNode, code: &[u8]) -> String {
    if let Some(name_node) = node.child_by_field_name("name")
        && let Ok(text) = name_node.utf8_text(code)
    {
        return text.to_string();
    }
    if let Some(parent) = node.parent() {
        for field in ["name", "left"] {
            if let Some(name_node) = parent.child_by_field_name(field)
                && let Ok(text) = name_node.utf8_text(code)
            {
                return text.to_string();
            }
        }
    }
    "<anonymous>".to_string()
}

fn named_child_of_kind<'a>(node: TsNode<'a>, kind: &str) -> Option<TsNode<'a>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn rust_visibility(node: TsNode, code: &[u8]) -> Visibility {
    match named_child_of_kind(node, "visibility_modifier").and_then(|n| n.utf8_text(code).ok()) {
        Some(text) if text.starts_with("pub(crate)") => Visibility::Crate,
        Some(_) => Visibility::Public,
        None => Visibility::Private,
    }
}

fn js_visibility(node: TsNode, code: &[u8]) -> Visibility {
    let modifier =
        named_child_of_kind(node, "accessibility_modifier").and_then(|n| n.utf8_text(code).ok());
    match modifier {
        Some("private") => return Visibility::Private,
        Some("protected") => return Visibility::Crate,
        Some("public") => return Visibility::Public,
        _ => {}
    }
    match node.parent() {
        Some(parent) if parent.kind() == "export_statement" => Visibility::Public,
        _ => Visibility::Private,
    }
}

fn java_visibility(node: TsNode, code: &[u8]) -> Visibility {
    let Some(modifiers) = named_child_of_kind(node, "modifiers") else {
        return Visibility::Crate;
    };
    let Ok(text) = modifiers.utf8_text(code) else {
        return Visibility::Crate;
    };
    if text.contains("private") {
        Visibility::Private
    } else if text.contains("public") {
        Visibility::Public
    } else {
        Visibility::Crate
    }
}

fn classify_visibility(spec: &LangSpec, node: TsNode, code: &[u8], name: &str) -> Visibility {
    match spec.language {
        Language::Rust => rust_visibility(node, code),
        Language::TypeScript | Language::JavaScript => js_visibility(node, code),
        Language::Python => {
            if name.starts_with('_') {
                Visibility::Private
            } else {
                Visibility::Public
            }
        }
        Language::Java => java_visibility(node, code),
        _ => Visibility::Private,
    }
}

fn collect_shells<'a>(
    node: TsNode<'a>,
    code: &[u8],
    spec: &LangSpec,
    qualifier: Option<String>,
    out: &mut Vec<Shell<'a>>,
) {
    let next_qualifier = qualifier_for(node, code, spec, qualifier);

    if spec.fn_kinds.contains(&node.kind()) {
        let name = extract_name(node, code);
        let visibility = classify_visibility(spec, node, code, &name);
        out.push(Shell {
            node,
            name,
            qualifier: next_qualifier.clone(),
            visibility,
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_shells(child, code, spec, next_qualifier.clone(), out);
    }
}

fn receiver_shaped(node: TsNode, code: &[u8], spec: &LangSpec) -> Option<(String, Option<String>)> {
    for (kind, object_field, prop_field) in spec.receiver_kinds {
        if node.kind() == *kind
            && let Some(prop_node) = node.child_by_field_name(prop_field)
            && let Ok(callee) = prop_node.utf8_text(code)
        {
            let receiver = node
                .child_by_field_name(object_field)
                .and_then(|n| n.utf8_text(code).ok())
                .map(str::to_string);
            return Some((callee.to_string(), receiver));
        }
    }
    None
}

fn fallback_callee(target: TsNode, code: &[u8]) -> (String, Option<String>) {
    let text = target.utf8_text(code).unwrap_or("<unknown>");
    let trailing = text.rsplit("::").next().unwrap_or(text);
    (trailing.to_string(), None)
}

fn try_extract_call(node: TsNode, code: &[u8], spec: &LangSpec) -> Option<CallSite> {
    for (kind, callee_field) in spec.call_kinds {
        if node.kind() != *kind {
            continue;
        }
        // Java's `method_invocation` carries `object`/`name` directly on itself, unlike
        // Rust/JS/Python where the receiver lives on a nested callee expression.
        let (callee, receiver) = if let Some(hit) = receiver_shaped(node, code, spec) {
            hit
        } else {
            let target = node.child_by_field_name(callee_field)?;
            receiver_shaped(target, code, spec).unwrap_or_else(|| fallback_callee(target, code))
        };
        return Some(CallSite {
            callee,
            receiver,
            line: node.start_position().row + 1,
        });
    }
    None
}

fn collect_calls_in_body(node: TsNode, code: &[u8], spec: &LangSpec, out: &mut Vec<CallSite>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if spec.fn_kinds.contains(&child.kind()) {
            continue;
        }
        if let Some(call) = try_extract_call(child, code, spec) {
            out.push(call);
        }
        collect_calls_in_body(child, code, spec, out);
    }
}

fn finalize_shell(shell: Shell, code: &[u8], spec: &LangSpec) -> FunctionNode {
    let node = shell.node;
    let mut calls = Vec::new();
    let mut body_statements = 0;
    if let Some(body) = node.child_by_field_name("body") {
        collect_calls_in_body(body, code, spec, &mut calls);
        body_statements = if BLOCK_KINDS.contains(&body.kind()) {
            body.named_child_count()
        } else {
            1
        };
    }
    FunctionNode {
        name: shell.name,
        qualifier: shell.qualifier,
        visibility: shell.visibility,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        body_statements,
        calls,
    }
}

pub fn parse_source_tree(path: &Path) -> Option<SourceTree> {
    let language = Language::from_path(path);
    lang_spec(language)?;
    // rca's own get_language_for_file unwraps a non-UTF-8 extension; reject it first.
    path.extension().and_then(|e| e.to_str())?;
    let lang = get_language_for_file(path)?;
    let source = std::fs::read(path).ok()?;
    let functions = action::<SourceTreeCallback>(&lang, source, path, None, language)?;
    Some(SourceTree {
        path: path.to_path_buf(),
        language,
        functions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_and_parse(tmp: &TempDir, name: &str, content: &str) -> SourceTree {
        let file = tmp.path().join(name);
        fs::write(&file, content).unwrap();
        parse_source_tree(&file).unwrap_or_else(|| panic!("failed to parse {name}"))
    }

    #[test]
    fn rust_visibility_variants() {
        let tmp = TempDir::new().unwrap();
        let tree = write_and_parse(
            &tmp,
            "vis.rs",
            "pub fn a() {}\nfn b() {}\npub(crate) fn c() {}\n",
        );
        assert_eq!(tree.functions.len(), 3);
        assert_eq!(tree.functions[0].name, "a");
        assert_eq!(tree.functions[0].visibility, Visibility::Public);
        assert_eq!(tree.functions[1].name, "b");
        assert_eq!(tree.functions[1].visibility, Visibility::Private);
        assert_eq!(tree.functions[2].name, "c");
        assert_eq!(tree.functions[2].visibility, Visibility::Crate);
    }

    #[test]
    fn rust_method_qualifier() {
        let tmp = TempDir::new().unwrap();
        let tree = write_and_parse(
            &tmp,
            "impl_test.rs",
            "struct Foo;\nimpl Foo {\n    pub fn bar(&self) {}\n}\n",
        );
        let bar = tree.functions.iter().find(|f| f.name == "bar").unwrap();
        assert_eq!(bar.qualifier.as_deref(), Some("Foo"));
    }

    #[test]
    fn rust_call_extraction_direct_and_method() {
        let tmp = TempDir::new().unwrap();
        let tree = write_and_parse(
            &tmp,
            "calls.rs",
            "fn helper() {}\nfn caller(x: Vec<i32>) {\n    helper();\n    x.len();\n}\n",
        );
        let caller = tree.functions.iter().find(|f| f.name == "caller").unwrap();
        assert!(
            caller
                .calls
                .iter()
                .any(|c| c.callee == "helper" && c.receiver.is_none())
        );
        let method_call = caller
            .calls
            .iter()
            .find(|c| c.callee == "len")
            .expect("method call recorded");
        assert_eq!(method_call.receiver.as_deref(), Some("x"));
    }

    #[test]
    fn rust_body_statements_count() {
        let tmp = TempDir::new().unwrap();
        let tree = write_and_parse(
            &tmp,
            "body.rs",
            "fn one() {\n    let x = 1;\n}\nfn many() {\n    let a = 1;\n    let b = 2;\n    let c = a + b;\n}\n",
        );
        let one = tree.functions.iter().find(|f| f.name == "one").unwrap();
        let many = tree.functions.iter().find(|f| f.name == "many").unwrap();
        assert_eq!(one.body_statements, 1);
        assert_eq!(many.body_statements, 3);
    }

    #[test]
    fn rust_line_numbers_are_one_based_and_correct() {
        let tmp = TempDir::new().unwrap();
        let tree = write_and_parse(
            &tmp,
            "lines.rs",
            "\n\nfn third_line() {\n    let x = 1;\n}\n",
        );
        let f = &tree.functions[0];
        assert_eq!(f.start_line, 3);
        assert_eq!(f.end_line, 5);
    }

    #[test]
    fn java_visibility_and_calls() {
        let tmp = TempDir::new().unwrap();
        let tree = write_and_parse(
            &tmp,
            "Sample.java",
            "class Sample {\n    public void pubMethod() {}\n    private void privMethod() {}\n    void pkgMethod() {\n        pubMethod();\n        obj.doThing();\n    }\n}\n",
        );
        let pub_m = tree
            .functions
            .iter()
            .find(|f| f.name == "pubMethod")
            .unwrap();
        assert_eq!(pub_m.visibility, Visibility::Public);
        assert_eq!(pub_m.qualifier.as_deref(), Some("Sample"));
        let priv_m = tree
            .functions
            .iter()
            .find(|f| f.name == "privMethod")
            .unwrap();
        assert_eq!(priv_m.visibility, Visibility::Private);
        let pkg_m = tree
            .functions
            .iter()
            .find(|f| f.name == "pkgMethod")
            .unwrap();
        assert_eq!(pkg_m.visibility, Visibility::Crate);
        assert!(pkg_m.calls.iter().any(|c| c.callee == "pubMethod"));
        let method_call = pkg_m
            .calls
            .iter()
            .find(|c| c.callee == "doThing")
            .expect("method call recorded");
        assert_eq!(method_call.receiver.as_deref(), Some("obj"));
    }

    #[test]
    fn unsupported_extension_returns_none() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("notes.txt");
        fs::write(&file, "just text").unwrap();
        assert!(parse_source_tree(&file).is_none());
    }

    #[test]
    fn typescript_functions_and_calls() {
        let tmp = TempDir::new().unwrap();
        let tree = write_and_parse(
            &tmp,
            "sample.ts",
            "export function pub_fn() {}\nfunction priv_fn() {}\nfunction caller() {\n    pub_fn();\n    console.log(1);\n}\n",
        );
        let pub_fn = tree.functions.iter().find(|f| f.name == "pub_fn").unwrap();
        assert_eq!(pub_fn.visibility, Visibility::Public);
        let priv_fn = tree.functions.iter().find(|f| f.name == "priv_fn").unwrap();
        assert_eq!(priv_fn.visibility, Visibility::Private);
        let caller = tree.functions.iter().find(|f| f.name == "caller").unwrap();
        assert!(caller.calls.iter().any(|c| c.callee == "pub_fn"));
        let log_call = caller
            .calls
            .iter()
            .find(|c| c.callee == "log")
            .expect("method call recorded");
        assert_eq!(log_call.receiver.as_deref(), Some("console"));
    }

    #[test]
    fn python_visibility_and_calls() {
        let tmp = TempDir::new().unwrap();
        let tree = write_and_parse(
            &tmp,
            "sample.py",
            "def public_fn():\n    pass\n\ndef _private_fn():\n    pass\n\ndef caller():\n    public_fn()\n    obj.method()\n",
        );
        let public_fn = tree
            .functions
            .iter()
            .find(|f| f.name == "public_fn")
            .unwrap();
        assert_eq!(public_fn.visibility, Visibility::Public);
        let private_fn = tree
            .functions
            .iter()
            .find(|f| f.name == "_private_fn")
            .unwrap();
        assert_eq!(private_fn.visibility, Visibility::Private);
        let caller = tree.functions.iter().find(|f| f.name == "caller").unwrap();
        assert!(caller.calls.iter().any(|c| c.callee == "public_fn"));
        let method_call = caller
            .calls
            .iter()
            .find(|c| c.callee == "method")
            .expect("method call recorded");
        assert_eq!(method_call.receiver.as_deref(), Some("obj"));
    }
}
