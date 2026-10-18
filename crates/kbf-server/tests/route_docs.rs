//! The drift guard: every route the operator router serves has a row in `docs/api.md` and in
//! `docs/design/operator-verbs.md`, method and path both.
//!
//! The routes are read from the `.route(` calls of this crate's sources, constants resolved
//! within their own file, so a route added to any of the merged routers goes red here until the
//! two documents name it. Proven by:
//! - `an_undocumented_route_is_reported` (a planted route, a planted method),
//! - deleting the `GET /healthz` row of `operator-verbs.md`, which turns
//!   `every_route_is_in_the_verbs_table` red (see the pull request for the run).
//!
//! What it does not check: the gate column of the verbs table (that lives in the handlers) and
//! the reverse direction (a documented row with no route, which `[V]` and `[N]` rows are by
//! design).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

type Route = (String, String);

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            && !path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("tests.rs")
        {
            out.push(path);
        }
    }
}

/// `{name}` becomes `{}` and a trailing `:verb` goes, so `/v1/nodes/{target}` and
/// `/v1/nodes/{node}:cordon` compare equal.
fn normalise(path: &str) -> String {
    let mut out = String::new();
    let mut in_brace = false;
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'*') => {
                in_brace = true;
                out.push_str("{*rest}");
            }
            '{' => {
                in_brace = true;
                out.push_str("{}");
            }
            '}' => in_brace = false,
            _ if in_brace => {}
            _ => out.push(c),
        }
    }
    if let Some(colon) = out.rfind(':') {
        let tail = &out[colon + 1..];
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_lowercase()) {
            out.truncate(colon);
        }
    }
    out
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Every `const NAME: &str = "...";` of one file.
fn consts(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in src.lines() {
        let line = line.trim();
        let Some(at) = line.find("const ") else {
            continue;
        };
        let rest = &line[at + 6..];
        let Some((name, value)) = rest.split_once(": &str = \"") else {
            continue;
        };
        if let Some((value, _)) = value.split_once('"') {
            out.push((name.trim().to_string(), value.to_string()));
        }
    }
    out
}

/// The `(METHOD, normalised path)` of every `.route(` call in `src`.
fn code_routes(src: &str) -> Vec<Route> {
    let uncommented: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let consts = consts(&uncommented);
    let mut routes = Vec::new();
    let mut rest = uncommented.as_str();
    while let Some(at) = rest.find(".route(") {
        let args = &rest[at + 7..];
        rest = args;
        // The arguments up to the matching close paren, strings skipped.
        let mut depth = 1;
        let mut in_str = false;
        let mut end = args.len();
        for (i, c) in args.char_indices() {
            match c {
                '"' => in_str = !in_str,
                '(' if !in_str => depth += 1,
                ')' if !in_str => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let args = &args[..end];
        let (first, handlers) = args
            .split_once(',')
            .expect("a route has a path and handlers");
        let first = first.trim();
        let path = match first.strip_prefix('"') {
            Some(lit) => lit.trim_end_matches('"').to_string(),
            None => consts
                .iter()
                .find(|(name, _)| name == first)
                .unwrap_or_else(|| {
                    panic!("route path `{first}` is neither a literal nor a const of its file")
                })
                .1
                .clone(),
        };
        let path = normalise(&path);
        for method in ["get", "post", "put", "delete"] {
            let needle = format!("{method}(");
            let mut from = 0;
            while let Some(i) = handlers[from..].find(&needle) {
                let i = from + i;
                let before = handlers[..i].chars().next_back();
                if !before.is_some_and(is_ident) {
                    routes.push((method.to_uppercase(), path.clone()));
                }
                from = i + needle.len();
            }
        }
    }
    routes
}

/// The `(METHOD, normalised path)` pairs a document writes as `METHOD /path`.
fn doc_routes(text: &str) -> BTreeSet<Route> {
    let mut out = BTreeSet::new();
    for method in ["GET", "POST", "PUT", "DELETE"] {
        let needle = format!("{method} /");
        let mut from = 0;
        while let Some(i) = text[from..].find(&needle) {
            let i = from + i;
            from = i + needle.len();
            if text[..i].chars().next_back().is_some_and(is_ident) {
                continue;
            }
            let path: String = text[i + method.len() + 1..]
                .chars()
                .take_while(|c| !c.is_whitespace() && !"`?,)\"'".contains(*c))
                .collect();
            out.insert((method.to_string(), normalise(path.trim_end_matches('.'))));
        }
    }
    out
}

/// The routes of `code` that `doc` does not name. A catch-all `{*rest}` is named by any row under
/// its prefix.
fn missing(code: &[Route], doc: &BTreeSet<Route>) -> Vec<Route> {
    code.iter()
        .filter(|(method, path)| match path.strip_suffix("/{*rest}") {
            Some(prefix) => !doc
                .iter()
                .any(|(m, p)| m == method && p.starts_with(&format!("{prefix}/"))),
            None => !doc.contains(&(method.clone(), path.clone())),
        })
        .cloned()
        .collect()
}

fn server_routes() -> Vec<Route> {
    let mut files = Vec::new();
    rust_files(&root().join("crates/kbf-server/src"), &mut files);
    let mut routes: Vec<Route> = files
        .iter()
        .flat_map(|f| code_routes(&fs::read_to_string(f).unwrap()))
        .collect();
    routes.sort();
    routes.dedup();
    routes
}

fn check(doc_path: &str) {
    let doc = doc_routes(&fs::read_to_string(root().join(doc_path)).unwrap());
    let routes = server_routes();
    assert!(
        routes.len() > 25,
        "the scan found only {} routes: it is not reading the sources",
        routes.len()
    );
    let missing = missing(&routes, &doc);
    assert!(
        missing.is_empty(),
        "{doc_path} does not name these routes: {missing:?}"
    );
}

#[test]
fn every_route_is_in_the_api_doc() {
    check("docs/api.md");
}

#[test]
fn every_route_is_in_the_verbs_table() {
    check("docs/design/operator-verbs.md");
}

#[test]
fn an_undocumented_route_is_reported() {
    let src = r#"
        const PLANTED: &str = "/v1/planted/{id}";
        fn router() -> Router {
            Router::new()
                .route("/v1/nodes", get(a))
                .route(PLANTED, get(b).post(c))
                .route("/v1/catch/{id}/{*rest}", get(d))
        }
    "#;
    let code = code_routes(src);
    assert_eq!(
        code,
        [
            ("GET".to_string(), "/v1/nodes".to_string()),
            ("GET".to_string(), "/v1/planted/{}".to_string()),
            ("POST".to_string(), "/v1/planted/{}".to_string()),
            ("GET".to_string(), "/v1/catch/{}/{*rest}".to_string()),
        ]
    );
    let doc =
        doc_routes("`GET /v1/nodes` and `GET /v1/planted/{name}` and `GET /v1/catch/{x}/logs`");
    // POST is the only one missing: the method is part of the key, and the catch-all is named by
    // a row under its prefix.
    assert_eq!(
        missing(&code, &doc),
        [("POST".to_string(), "/v1/planted/{}".to_string())]
    );
    assert_eq!(missing(&code, &doc_routes("`GET /v1/nodes`")).len(), 3);
}

#[test]
fn the_doc_reader_strips_verbs_queries_and_punctuation() {
    let doc = doc_routes(
        "`POST /v1/nodes/{node}:cordon`, `GET /v1/shapes/{key}?instance=&cfg=` and GET /healthz.",
    );
    let want: BTreeSet<Route> = [
        ("POST", "/v1/nodes/{}"),
        ("GET", "/v1/shapes/{}"),
        ("GET", "/healthz"),
    ]
    .map(|(m, p)| (m.to_string(), p.to_string()))
    .into();
    assert_eq!(doc, want);
}
