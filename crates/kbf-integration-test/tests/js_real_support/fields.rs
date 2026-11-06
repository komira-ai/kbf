//! The set of fields an answer carries on the wire, named by `js-proto`'s own descriptor set.
//!
//! The answer is encoded (`prost::Message::encode_to_vec`, as the client decoded it) and its
//! bytes are walked: each field present is named by its path from the top message
//! (`spec.needs.cpu_millis`, `files[].name`, `env{}` for a map with an entry), nested messages
//! are walked into, and a field number the descriptor does not know is named `#<number>`. A
//! proto3 field left at its default is not on the wire, so it is not in the set; an `optional`
//! field that is set is, whatever its value. Two servers whose answers to the same calls differ
//! in which fields they set, or carry a value under another field, give different sets.

use std::collections::{BTreeMap, BTreeSet};

use prost::Message as _;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, FileDescriptorSet};

/// The messages of the job service's package, by full name (`.jobscheduler.v1.Job`).
pub struct Schema {
    messages: BTreeMap<String, DescriptorProto>,
}

/// How a field is named in a path, and what is walked inside it.
struct Field<'a> {
    /// The path segment: the name, with `[]` for a repeated field and `{}` for a map.
    segment: String,
    /// The full name of the message walked into: a message field that is not a map.
    inner: Option<&'a str>,
}

impl Schema {
    /// The schema `js-proto` was generated from.
    pub fn job_service() -> Schema {
        let set = FileDescriptorSet::decode(js_proto::FILE_DESCRIPTOR_SET)
            .expect("js-proto's descriptor set decodes");
        let mut messages = BTreeMap::new();
        for file in set.file {
            let scope = format!(".{}", file.package());
            for message in file.message_type {
                add(&mut messages, &scope, message);
            }
        }
        Schema { messages }
    }

    /// The full name of the message `short` of `jobscheduler.v1` (`Job`), checked to exist.
    pub fn message(&self, short: &str) -> String {
        let name = format!(".jobscheduler.v1.{short}");
        assert!(
            self.messages.contains_key(&name),
            "jobscheduler.v1 has no message {short}"
        );
        name
    }

    /// The field of `message` numbered `number`, if the descriptor has it.
    fn field(&self, message: &str, number: u64) -> Option<Field<'_>> {
        let found = self
            .messages
            .get(message)?
            .field
            .iter()
            .find(|field| field.number.and_then(|n| u64::try_from(n).ok()) == Some(number))?;
        let inner = (found.r#type() == Type::Message).then(|| found.type_name());
        let map = inner.is_some_and(|name| {
            self.messages
                .get(name)
                .and_then(|entry| entry.options.as_ref())
                .is_some_and(|options| options.map_entry())
        });
        let suffix = match (map, found.label() == Label::Repeated) {
            (true, _) => "{}",
            (false, true) => "[]",
            (false, false) => "",
        };
        Some(Field {
            segment: format!("{}{suffix}", found.name()),
            inner: inner.filter(|_| !map),
        })
    }

    /// The paths of the fields `value`, a message of the full name `message`, carries.
    pub fn fields(&self, message: &str, value: &impl prost::Message) -> BTreeSet<String> {
        self.fields_of(message, &value.encode_to_vec())
    }

    /// The paths of the fields the encoded message `bytes` of the full name `message` carries.
    pub fn fields_of(&self, message: &str, bytes: &[u8]) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        self.walk(message, bytes, "", &mut out);
        out
    }

    fn walk(&self, message: &str, mut bytes: &[u8], prefix: &str, out: &mut BTreeSet<String>) {
        while !bytes.is_empty() {
            let key = varint(&mut bytes);
            let number = key >> 3;
            let payload = match key & 7 {
                0 => {
                    varint(&mut bytes);
                    None
                }
                1 => {
                    take(&mut bytes, 8);
                    None
                }
                2 => {
                    let len = usize::try_from(varint(&mut bytes)).expect("a length fits");
                    Some(take(&mut bytes, len))
                }
                5 => {
                    take(&mut bytes, 4);
                    None
                }
                other => panic!("wire type {other} in {message}: proto3 has no groups"),
            };
            let Some(field) = self.field(message, number) else {
                out.insert(format!("{prefix}#{number}"));
                continue;
            };
            let path = format!("{prefix}{}", field.segment);
            if let (Some(inner), Some(payload)) = (field.inner, payload) {
                self.walk(inner, payload, &format!("{path}."), out);
            }
            out.insert(path);
        }
    }
}

/// Adds `message`, and the messages nested in it, under `scope`.
fn add(messages: &mut BTreeMap<String, DescriptorProto>, scope: &str, message: DescriptorProto) {
    let name = format!("{scope}.{}", message.name());
    for nested in &message.nested_type {
        add(messages, &name, nested.clone());
    }
    messages.insert(name, message);
}

/// Reads a varint off the front of `bytes`.
fn varint(bytes: &mut &[u8]) -> u64 {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = bytes.split_first().expect("a whole varint");
        *bytes = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
    }
    panic!("a varint longer than ten bytes")
}

/// Takes `len` bytes off the front of `bytes`.
fn take<'a>(bytes: &mut &'a [u8], len: usize) -> &'a [u8] {
    assert!(
        bytes.len() >= len,
        "a field runs past the end of its message"
    );
    let (head, rest) = bytes.split_at(len);
    *bytes = rest;
    head
}
