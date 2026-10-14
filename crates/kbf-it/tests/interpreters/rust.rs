//! The `[src-sh-c]` scan: production Rust under `crates/*/src/` that names a shell and
//! a `-c` flag.
//!
//! Each file is parsed with `syn`. A file counts when its production code holds a
//! string literal that is a shell ([`is_shell`]: `sh`, `bash`, `ash`, `dash`, `ksh` or
//! `zsh`, bare or as an absolute path) and a string literal that is a `-c` flag
//! ([`is_dash_c`]: `-c`, `-ec`, `-lc`, ...), anywhere in the file: the two need not
//! meet in one call, so `Command::new(shell).arg("-c")` with
//! `shell = Path::new("/bin/sh")` counts.
//! Literals inside macro calls (`vec!`, `format!`) are read from the macro's tokens;
//! doc comments are not code and are skipped.
//!
//! Test code is left out: an item, impl item, trait item or `let` under a
//! `#[cfg(...)]` that holds only with `test` (`test`, or `all(...)` with `test` in
//! it), a file whose inner attributes say so, and a file reached only through a
//! `mod` declared under such a `cfg` (followed through `#[path]` and nested modules).
//!
//! What it cannot see: a shell name or flag built at run time (`concat!`, `format!`
//! pieces, a value read from a file), and a `#[cfg(test)]` on an expression, whose
//! code it counts as production (the safe side).
//!
//! The same walk serves the `[test-interpreters]` scan ([`scan`]): test code (all of
//! `crates/*/tests/`, and the test code of `crates/*/src/` as above) that names an
//! interpreter in a string literal ([`named_interpreter`]): a program literal such as
//! `"perl"` or `"/bin/sh"`, or a script whose `#!` line runs one. A literal names the
//! program whatever it is passed to (`Command::new`, an argv array, a helper), so a
//! test that only uses the name as data is found too; its row says so. String, byte
//! string and C string literals are read. It cannot see a name built at run time
//! (`concat!`, `format!` pieces), a script read from a file, a name inside a longer
//! literal (`"sh -c x"`, `"/bin/sh x"`), or code outside `src/` and `tests/`
//! (examples, build scripts).

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::{TokenStream, TokenTree};
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Attribute, ImplItem, Item, ItemMod, Lit, Local, Meta, Token, TraitItem};

/// A shell program name, bare (`sh`) or as an absolute path (`/bin/sh`).
pub fn is_shell(s: &str) -> bool {
    let name = s.rsplit('/').next().unwrap_or(s);
    (!s.contains('/') || s.starts_with('/'))
        && matches!(name, "sh" | "bash" | "ash" | "dash" | "ksh" | "zsh")
}

/// A shell's command-string flag: `-` and letters ending in `c` (`-c`, `-ec`, `-lc`).
pub fn is_dash_c(s: &str) -> bool {
    s.strip_prefix('-')
        .is_some_and(|flags| flags.ends_with('c') && flags.bytes().all(|b| b.is_ascii_alphabetic()))
}

/// Whether a `cfg` predicate holds only when `test` does.
fn test_only_predicate(meta: &Meta) -> bool {
    match meta {
        Meta::Path(p) => p.is_ident("test"),
        Meta::List(list) if list.path.is_ident("all") => list
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .is_ok_and(|all| all.iter().any(test_only_predicate)),
        Meta::List(list) if list.path.is_ident("any") => list
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .is_ok_and(|any| !any.is_empty() && any.iter().all(test_only_predicate)),
        _ => false,
    }
}

/// Whether the attributes hold a `#[cfg(...)]` (or `#![cfg(...)]`) that holds only
/// with `test`.
pub fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && a.parse_args::<Meta>()
                .is_ok_and(|m| test_only_predicate(&m))
    })
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

fn impl_item_attrs(item: &ImplItem) -> &[Attribute] {
    match item {
        ImplItem::Const(i) => &i.attrs,
        ImplItem::Fn(i) => &i.attrs,
        ImplItem::Type(i) => &i.attrs,
        ImplItem::Macro(i) => &i.attrs,
        _ => &[],
    }
}

fn trait_item_attrs(item: &TraitItem) -> &[Attribute] {
    match item {
        TraitItem::Const(i) => &i.attrs,
        TraitItem::Fn(i) => &i.attrs,
        TraitItem::Type(i) => &i.attrs,
        TraitItem::Macro(i) => &i.attrs,
        _ => &[],
    }
}

/// An interpreter program a string literal names, as its base name: the whole
/// literal a program (bare, or an absolute path) that [`super::is_interpreter`] knows
/// or `osascript`, or a script whose `#!` line runs one. `None` otherwise.
pub fn named_interpreter(value: &str) -> Option<&str> {
    let spawned = |name: &str| super::is_interpreter(name) || name == "osascript";
    let name = value.rsplit('/').next().unwrap_or(value);
    if (!value.contains('/') || value.starts_with('/')) && spawned(name) {
        return Some(name);
    }
    super::shebang_interpreter(value.as_bytes()).filter(|i| spawned(i))
}

/// The name of an item, for the list of items a finding names: a function, const,
/// static, module, trait, macro call, or the type an `impl` block is for.
fn item_name(item: &Item) -> Option<String> {
    match item {
        Item::Fn(i) => Some(i.sig.ident.to_string()),
        Item::Const(i) => Some(i.ident.to_string()),
        Item::Static(i) => Some(i.ident.to_string()),
        Item::Mod(i) => Some(i.ident.to_string()),
        Item::Trait(i) => Some(i.ident.to_string()),
        Item::Impl(i) => match &*i.self_ty {
            syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
            _ => None,
        },
        Item::Macro(i) => match &i.ident {
            Some(name) => Some(name.to_string()),
            None => i.mac.path.segments.last().map(|s| format!("{}!", s.ident)),
        },
        _ => None,
    }
}

fn impl_item_name(item: &ImplItem) -> Option<String> {
    match item {
        ImplItem::Fn(i) => Some(i.sig.ident.to_string()),
        ImplItem::Const(i) => Some(i.ident.to_string()),
        _ => None,
    }
}

fn trait_item_name(item: &TraitItem) -> Option<String> {
    match item {
        TraitItem::Fn(i) => Some(i.sig.ident.to_string()),
        TraitItem::Const(i) => Some(i.ident.to_string()),
        _ => None,
    }
}

/// An interpreter a literal names, where it was found, and whether the code around
/// it was test code.
struct Named {
    interpreter: String,
    item: String,
    test: bool,
}

/// A `mod name;` declaration: the files it may name, and whether it is test code.
struct Decl {
    candidates: Vec<String>,
    test: bool,
}

/// One file's walk: its shell and `-c` literals in production code, and its module
/// declarations.
struct Walk {
    /// The directory a `mod x;` at the top of this file resolves in.
    child_dir: String,
    /// The directory holding this file (where a top-level `#[path]` resolves).
    file_dir: String,
    /// Inline modules entered so far.
    inline: Vec<String>,
    in_test: bool,
    /// The file's inner attributes make all of it test code.
    file_test: bool,
    /// Production code holds a shell literal and a `-c` literal.
    shell: bool,
    dash_c: bool,
    decls: Vec<Decl>,
    /// The named items entered so far (functions, modules, `impl` types).
    items: Vec<String>,
    /// Every literal that names an interpreter, test code or not.
    names: Vec<Named>,
}

impl Walk {
    fn literal(&mut self, value: &str) {
        if let Some(interpreter) = named_interpreter(value) {
            let item = if self.items.is_empty() {
                "(file)".to_owned()
            } else {
                self.items.join("::")
            };
            self.names.push(Named {
                interpreter: interpreter.to_owned(),
                item,
                test: self.in_test,
            });
        }
        if self.in_test {
            return;
        }
        self.shell |= is_shell(value);
        self.dash_c |= is_dash_c(value);
    }

    fn tokens(&mut self, tokens: TokenStream) {
        for tree in tokens {
            match tree {
                TokenTree::Group(g) => self.tokens(g.stream()),
                TokenTree::Literal(l) => {
                    if let Some(text) = lit_text(&Lit::new(l)) {
                        self.literal(&text);
                    }
                }
                TokenTree::Ident(_) | TokenTree::Punct(_) => {}
            }
        }
    }

    /// Runs `f` inside the item `name` (when it has one), with test code marked when
    /// `attrs` say the node is test-only.
    fn under(&mut self, name: Option<String>, attrs: &[Attribute], f: impl FnOnce(&mut Self)) {
        let was = self.in_test;
        let named = name.is_some();
        self.items.extend(name);
        self.in_test |= test_only(attrs);
        f(self);
        self.in_test = was;
        if named {
            self.items.pop();
        }
    }

    fn base(&self) -> String {
        let mut base = self.child_dir.clone();
        for m in &self.inline {
            base = format!("{base}/{m}");
        }
        base
    }
}

impl<'ast> Visit<'ast> for Walk {
    fn visit_attribute(&mut self, _: &'ast Attribute) {}

    fn visit_item(&mut self, item: &'ast Item) {
        self.under(item_name(item), item_attrs(item), |w| {
            visit::visit_item(w, item);
        });
    }

    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        self.under(impl_item_name(item), impl_item_attrs(item), |w| {
            visit::visit_impl_item(w, item);
        });
    }

    fn visit_trait_item(&mut self, item: &'ast TraitItem) {
        self.under(trait_item_name(item), trait_item_attrs(item), |w| {
            visit::visit_trait_item(w, item);
        });
    }

    fn visit_local(&mut self, local: &'ast Local) {
        self.under(None, &local.attrs, |w| visit::visit_local(w, local));
    }

    fn visit_lit(&mut self, lit: &'ast Lit) {
        if let Some(text) = lit_text(lit) {
            self.literal(&text);
        }
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        self.tokens(m.tokens.clone());
    }

    fn visit_item_mod(&mut self, m: &'ast ItemMod) {
        let name = m.ident.to_string();
        let path = m.attrs.iter().find_map(|a| match &a.meta {
            Meta::NameValue(nv) if nv.path.is_ident("path") => match &nv.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: Lit::Str(s), ..
                }) => Some(s.value()),
                _ => None,
            },
            _ => None,
        });
        match &m.content {
            None => {
                let candidates = match path {
                    Some(p) if self.inline.is_empty() => {
                        vec![normalize(&format!("{}/{p}", self.file_dir))]
                    }
                    Some(p) => vec![normalize(&format!("{}/{p}", self.base()))],
                    None => {
                        let base = self.base();
                        vec![format!("{base}/{name}.rs"), format!("{base}/{name}/mod.rs")]
                    }
                };
                self.decls.push(Decl {
                    candidates,
                    test: self.in_test,
                });
            }
            Some((_, items)) => {
                self.inline.push(path.unwrap_or(name));
                for item in items {
                    self.visit_item(item);
                }
                self.inline.pop();
            }
        }
    }
}

/// The text of a string, byte string or C string literal (a byte string that is not
/// UTF-8 read lossily), or `None` for any other literal.
fn lit_text(lit: &Lit) -> Option<String> {
    match lit {
        Lit::Str(s) => Some(s.value()),
        Lit::ByteStr(s) => Some(String::from_utf8_lossy(&s.value()).into_owned()),
        Lit::CStr(s) => Some(s.value().to_string_lossy().into_owned()),
        _ => None,
    }
}

/// `a/b/../c` as `a/c`, and `./` dropped.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// Where `mod x;` resolves for the file at `path`: its own directory for a crate or
/// module root (`lib.rs`, `main.rs`, `mod.rs`, a `src/bin/*.rs`), else
/// `<dir>/<stem>`.
fn child_dir(path: &str) -> (String, String) {
    let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
    let stem = file.strip_suffix(".rs").unwrap_or(file);
    let root = matches!(file, "lib.rs" | "main.rs" | "mod.rs") || dir.ends_with("/src/bin");
    let child = if root {
        dir.to_owned()
    } else {
        format!("{dir}/{stem}")
    };
    (child, dir.to_owned())
}

/// Whether `path` is a Rust file under a crate's `src/`: `crates/<crate>/src/...rs`.
pub fn is_crate_src(path: &str) -> bool {
    in_crate_dir(path, "src")
}

/// Whether `path` is a Rust file under a crate's `tests/`: `crates/<crate>/tests/...rs`.
/// All of it is test code.
pub fn is_crate_test(path: &str) -> bool {
    in_crate_dir(path, "tests")
}

fn in_crate_dir(path: &str, dir: &str) -> bool {
    let mut parts = path.split('/');
    parts.next() == Some("crates")
        && parts.next().is_some_and(|c| !c.is_empty())
        && parts.next() == Some(dir)
        && path.ends_with(".rs")
}

/// A test-code file that names interpreters: its path, and per interpreter (sorted)
/// the items that name it (sorted, each once; `(file)` for a literal outside any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestNames {
    pub path: String,
    pub names: BTreeMap<String, BTreeSet<String>>,
}

/// What [`scan`] found.
#[derive(Debug, Default)]
pub struct Scan {
    /// The production files under `crates/*/src/` that name a shell and a `-c` flag,
    /// sorted by path.
    pub shell_c: Vec<String>,
    /// The files whose test code names an interpreter, sorted by path.
    pub test_names: Vec<TestNames>,
    /// One per file that does not parse.
    pub problems: Vec<String>,
}

/// For `(path, text)` of every Rust file under the crates' `src/`, the production
/// files that name a shell and a `-c` flag, sorted by path; and one problem per file
/// that does not parse.
pub fn shell_c_files(files: &[(String, String)]) -> (Vec<String>, Vec<String>) {
    let scan = scan(files);
    (scan.shell_c, scan.problems)
}

/// Both scans over `(path, text)` of every Rust file under the crates' `src/` and
/// `tests/` ([`is_crate_src`], [`is_crate_test`]).
pub fn scan(files: &[(String, String)]) -> Scan {
    let mut problems = Vec::new();
    let mut walks: BTreeMap<&str, Walk> = BTreeMap::new();
    for (path, text) in files {
        let parsed = match syn::parse_file(text) {
            Ok(f) => f,
            Err(e) => {
                problems.push(format!(
                    "{path}: not read as Rust ({e}), so the shell scan cannot clear it"
                ));
                continue;
            }
        };
        let (child, file_dir) = child_dir(path);
        let file_test = is_crate_test(path) || test_only(&parsed.attrs);
        let mut walk = Walk {
            child_dir: child,
            file_dir,
            inline: Vec::new(),
            in_test: file_test,
            file_test,
            shell: false,
            dash_c: false,
            decls: Vec::new(),
            items: Vec::new(),
            names: Vec::new(),
        };
        walk.visit_file(&parsed);
        walks.insert(path, walk);
    }
    // A file is test code when its own inner attributes say so, or a `mod` declared
    // under test code (or in a test file) names it. Repeat until nothing changes.
    let mut test: BTreeSet<&str> = walks
        .iter()
        .filter(|(_, w)| w.file_test)
        .map(|(p, _)| *p)
        .collect();
    loop {
        let before = test.len();
        for (path, walk) in &walks {
            for decl in &walk.decls {
                if !(decl.test || test.contains(path)) {
                    continue;
                }
                if let Some(c) = decl
                    .candidates
                    .iter()
                    .find_map(|c| walks.get_key_value(c.as_str()).map(|(k, _)| *k))
                {
                    test.insert(c);
                }
            }
        }
        if test.len() == before {
            break;
        }
    }
    let shell_c = walks
        .iter()
        .filter(|(p, w)| !test.contains(*p) && w.shell && w.dash_c)
        .map(|(p, _)| (*p).to_owned())
        .collect();
    let mut test_names = Vec::new();
    for (path, walk) in &walks {
        let file_test = test.contains(path);
        let mut names: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for n in walk.names.iter().filter(|n| file_test || n.test) {
            names
                .entry(n.interpreter.clone())
                .or_default()
                .insert(n.item.clone());
        }
        if !names.is_empty() {
            test_names.push(TestNames {
                path: (*path).to_owned(),
                names,
            });
        }
    }
    Scan {
        shell_c,
        test_names,
        problems,
    }
}
