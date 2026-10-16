//! Nothing a daemon sends or receives can carry a cold-store endpoint, bucket or key.
//!
//! A daemon-only node holds no cold-store credential and reaches bytes only through
//! storage (docs/design/architecture-v2.md, section 9). The flags keep the credential
//! off its command line (kbf-cli's tests) and the layering keeps the object-store client
//! out of its binary (kbf-it's tests/layering.rs); this file keeps it off the wire: no
//! message of a service the daemon calls, and no message of the storage map that
//! `Welcome` and the map pushes carry, has a field or a type named for a cold store or its
//! credential, at any depth.
//!
//! The protos are compiled here as `build.rs` compiles them, and the walk starts from the
//! services' own method types, so an RPC added to the worker or segments service is
//! covered with no edit here.
//!
//! Catches:
//! - a field such as `string cold_store = 4;` added to `Welcome`, the map, a lease
//!   `Start` or any message they hold, however deep;
//! - a message type named for a cold store or its credential reachable from them under
//!   an innocent field name (`ColdStore store = 9;`);
//! - a walk that sees nothing: a service or the map renamed or removed, or a closure
//!   too small to hold `Welcome` and the map's `Member`.

use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;

use prost_types::field_descriptor_proto::Type;
use prost_types::{DescriptorProto, FileDescriptorSet};

/// The services a daemon calls: the work stream and the segment uploads and reads.
const DAEMON_SERVICES: &[&str] = &[".kbf.worker.v1.Worker", ".kbf.worker.v1.Segments"];

/// Messages a daemon receives outside those services' method types: the storage map,
/// which `Welcome` and the map pushes carry (docs/design/architecture-v2.md, section 6).
const DAEMON_MESSAGES: &[&str] = &[".kbf.map.v1.Map"];

/// Words no field or message name on the daemon's wire may hold, compared in lower case
/// with the separators removed (so `coldStore`, `cold_store` and `ColdStore` all match).
const COLD_STORE_WORDS: &[&str] = &[
    "cold",
    "bucket",
    "endpoint",
    "s3",
    "objstore",
    "objectstore",
    "credential",
    "secret",
    "accesskey",
    "password",
    "region",
];

fn descriptors() -> FileDescriptorSet {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("proto");
    let includes = [
        root.join("third_party/remote-apis"),
        root.join("third_party/googleapis"),
        root.join("third_party/grpc-proto"),
        root.clone(),
    ];
    protox::compile(
        [
            "kbf/worker/v1/worker.proto",
            "kbf/worker/v1/segments.proto",
            "kbf/map/v1/map.proto",
        ],
        includes,
    )
    .expect("the protos compile")
}

/// Every message of `set` by its full name (`.package.Outer.Inner`).
fn messages(set: &FileDescriptorSet) -> Vec<(String, &DescriptorProto)> {
    fn nested<'a>(
        prefix: &str,
        m: &'a DescriptorProto,
        out: &mut Vec<(String, &'a DescriptorProto)>,
    ) {
        let name = format!("{prefix}.{}", m.name());
        for inner in &m.nested_type {
            nested(&name, inner, out);
        }
        out.push((name, m));
    }
    let mut out = Vec::new();
    for file in &set.file {
        let prefix = if file.package().is_empty() {
            String::new()
        } else {
            format!(".{}", file.package())
        };
        for m in &file.message_type {
            nested(&prefix, m, &mut out);
        }
    }
    out
}

fn flat(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn cold_word(name: &str) -> Option<&'static str> {
    let name = flat(name);
    COLD_STORE_WORDS.iter().copied().find(|w| name.contains(w))
}

/// The message types the daemon's wire holds: the roots and every message type a field
/// of one names, transitively.
fn daemon_closure(set: &FileDescriptorSet) -> BTreeSet<String> {
    let all = messages(set);
    let by_name = |name: &str| all.iter().find(|(n, _)| n == name).map(|(_, m)| *m);
    let mut roots: Vec<String> = DAEMON_MESSAGES.iter().map(|s| (*s).to_owned()).collect();
    for service in DAEMON_SERVICES {
        let mut found = set.file.iter().flat_map(|f| {
            f.service
                .iter()
                .map(move |s| (format!(".{}.{}", f.package(), s.name()), s))
        });
        let (_, svc) = found
            .find(|(n, _)| n == *service)
            .unwrap_or_else(|| panic!("no service {service}"));
        assert!(!svc.method.is_empty(), "{service} has no methods");
        for method in &svc.method {
            roots.push(method.input_type().to_owned());
            roots.push(method.output_type().to_owned());
        }
    }
    let mut seen = BTreeSet::new();
    let mut queue: VecDeque<String> = roots.into();
    while let Some(name) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let m = by_name(&name).unwrap_or_else(|| panic!("no message {name}"));
        for field in &m.field {
            if field.r#type() == Type::Message {
                queue.push_back(field.type_name().to_owned());
            }
        }
    }
    seen
}

#[test]
fn the_daemon_wire_has_no_cold_store_field() {
    let set = descriptors();
    let closure = daemon_closure(&set);
    // The walk sees what it must: the work stream's Welcome and lease Start, the map and
    // its members, and the segment reads.
    for must in [
        ".kbf.worker.v1.Welcome",
        ".kbf.worker.v1.Start",
        ".kbf.map.v1.Map",
        ".kbf.map.v1.Member",
        ".kbf.worker.v1.ReadRangeResponse",
    ] {
        assert!(
            closure.contains(must),
            "{must} is not on the walk: {closure:?}"
        );
    }
    let all = messages(&set);
    let mut found = Vec::new();
    for name in &closure {
        let (_, m) = all
            .iter()
            .find(|(n, _)| n == name)
            .expect("the closure holds known messages");
        if let Some(word) = cold_word(m.name()) {
            found.push(format!("message {name} (\"{word}\")"));
        }
        for field in &m.field {
            if let Some(word) = cold_word(field.name()) {
                found.push(format!("{name}.{} (\"{word}\")", field.name()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "the daemon's wire names a cold store or its credential (a daemon-only node holds \
         none, docs/design/architecture-v2.md section 9):\n{}",
        found.join("\n")
    );
}

/// Catches a matcher that cannot fire: each spelling of a cold-store name a proto could
/// use is found, and the names on today's wire that are not one are not.
#[test]
fn the_cold_store_matcher_fires_on_each_spelling() {
    for name in [
        "cold_store",
        "ColdStore",
        "coldStoreUri",
        "s3_endpoint",
        "bucket",
        "object_store",
        "ObjStoreConfig",
        "aws_access_key_id",
        "secret_access_key",
        "credentials",
        "region",
    ] {
        assert!(cold_word(name).is_some(), "{name}");
    }
    for name in [
        "addr",
        "zone",
        "weight",
        "epoch",
        "object",
        "denied",
        "action_digest",
    ] {
        assert_eq!(cold_word(name), None, "{name}");
    }
}
