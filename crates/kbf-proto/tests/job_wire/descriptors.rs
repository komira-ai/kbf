//! The earlier `worker.proto` (`tests/fixtures/worker_before_job.proto`) against today's: the
//! descriptors of the two files compared, so that nothing of the earlier file is removed or
//! changed and the only additions are the ones listed. Moved here from `job_wire.rs`, which it
//! is a module of, as it was.

use std::collections::BTreeSet;
use std::path::PathBuf;

use prost_types::{FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet};

fn compile(file: &str, first: Option<PathBuf>) -> FileDescriptorSet {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("proto");
    let mut includes: Vec<PathBuf> = first.into_iter().collect();
    includes.extend([
        root.join("third_party/remote-apis"),
        root.join("third_party/googleapis"),
        root.join("third_party/grpc-proto"),
        root.clone(),
    ]);
    protox::compile([file], includes).expect("the proto compiles")
}

fn file_of(set: FileDescriptorSet, name: &str) -> FileDescriptorProto {
    set.file
        .into_iter()
        .find(|f| f.name() == name)
        .unwrap_or_else(|| panic!("no file {name}"))
}

fn before() -> FileDescriptorProto {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let name = "worker_before_job.proto";
    file_of(compile(name, Some(fixtures)), name)
}

fn today() -> FileDescriptorProto {
    let name = "kbf/worker/v1/worker.proto";
    file_of(compile(name, None), name)
}

fn shape(f: &FieldDescriptorProto) -> String {
    format!(
        "{} {:?} {:?} {} {} {:?}",
        f.name(),
        f.r#type(),
        f.label(),
        f.type_name(),
        f.proto3_optional(),
        f.oneof_index
    )
}

/// What `new` removed, renumbered or retyped of `old`: messages (and their oneofs), fields
/// by number, enums and their values by number, and the methods of services.
fn breaks(old: &FileDescriptorProto, new: &FileDescriptorProto) -> Vec<String> {
    let mut out = Vec::new();
    for om in &old.message_type {
        let Some(nm) = new.message_type.iter().find(|m| m.name() == om.name()) else {
            out.push(format!("message {} removed", om.name()));
            continue;
        };
        let oneofs = |m: &prost_types::DescriptorProto| {
            m.oneof_decl
                .iter()
                .map(|o| o.name().to_owned())
                .collect::<Vec<_>>()
        };
        if !oneofs(nm).starts_with(&oneofs(om)) {
            out.push(format!("message {} oneofs changed", om.name()));
        }
        for of in &om.field {
            match nm.field.iter().find(|f| f.number() == of.number()) {
                None => out.push(format!("{}.{} removed or renumbered", om.name(), of.name())),
                Some(nf) if shape(nf) != shape(of) => out.push(format!(
                    "{}.{} changed: {} -> {}",
                    om.name(),
                    of.name(),
                    shape(of),
                    shape(nf)
                )),
                Some(_) => {}
            }
        }
    }
    for oe in &old.enum_type {
        let Some(ne) = new.enum_type.iter().find(|e| e.name() == oe.name()) else {
            out.push(format!("enum {} removed", oe.name()));
            continue;
        };
        for ov in &oe.value {
            let kept = ne
                .value
                .iter()
                .any(|v| v.number() == ov.number() && v.name() == ov.name());
            if !kept {
                out.push(format!("{}.{} removed or renumbered", oe.name(), ov.name()));
            }
        }
    }
    for os in &old.service {
        let Some(ns) = new.service.iter().find(|s| s.name() == os.name()) else {
            out.push(format!("service {} removed", os.name()));
            continue;
        };
        for om in &os.method {
            let kept = ns.method.iter().any(|m| {
                (
                    m.name(),
                    m.input_type(),
                    m.output_type(),
                    m.client_streaming(),
                    m.server_streaming(),
                ) == (
                    om.name(),
                    om.input_type(),
                    om.output_type(),
                    om.client_streaming(),
                    om.server_streaming(),
                )
            });
            if !kept {
                out.push(format!("{}.{} removed or changed", os.name(), om.name()));
            }
        }
    }
    out
}

/// What `new` has that `old` has not, as sorted lines.
fn additions(old: &FileDescriptorProto, new: &FileDescriptorProto) -> Vec<String> {
    let mut out = BTreeSet::new();
    for nm in &new.message_type {
        match old.message_type.iter().find(|m| m.name() == nm.name()) {
            None => {
                out.insert(format!("message {}", nm.name()));
            }
            Some(om) => {
                for f in &nm.field {
                    if !om.field.iter().any(|o| o.number() == f.number()) {
                        out.insert(format!("field {}.{} = {}", nm.name(), f.name(), f.number()));
                    }
                }
            }
        }
    }
    for ne in &new.enum_type {
        match old.enum_type.iter().find(|e| e.name() == ne.name()) {
            None => {
                out.insert(format!("enum {}", ne.name()));
            }
            Some(oe) => {
                for v in &ne.value {
                    if !oe.value.iter().any(|o| o.number() == v.number()) {
                        out.insert(format!("value {}.{} = {}", ne.name(), v.name(), v.number()));
                    }
                }
            }
        }
    }
    for ns in &new.service {
        match old.service.iter().find(|s| s.name() == ns.name()) {
            None => {
                out.insert(format!("service {}", ns.name()));
            }
            Some(os) => {
                for m in &ns.method {
                    if !os.method.iter().any(|o| o.name() == m.name()) {
                        out.insert(format!("method {}.{}", ns.name(), m.name()));
                    }
                }
            }
        }
    }
    out.into_iter().collect()
}

#[test]
fn today_s_worker_proto_removes_and_changes_nothing_of_the_earlier_file() {
    let old = before();
    assert!(
        old.message_type.iter().all(|m| m.nested_type.is_empty()),
        "the comparison reads top-level messages only"
    );
    assert!(
        old.message_type.iter().any(|m| m.name() == "Start"),
        "the earlier file was read"
    );
    assert_eq!(breaks(&old, &today()), Vec::<String>::new());
}

#[test]
fn the_only_additions_to_the_worker_proto_are_the_job_fields_the_node_and_placement_load_the_cache_and_fallback_counts()
 {
    let mut want: Vec<String> = [
        "enum FencePolicy",
        "enum Isolation",
        "enum JobExit",
        // The node load a heartbeat carries (with the clocks and minutes of its leases) and the
        // peak window a lease's usage carries.
        "field CgroupUsage.peak_window_millicores = 17",
        "field Heartbeat.node_load = 6",
        "message NodeLoad",
        // The input cache counts a heartbeat carries.
        "field Heartbeat.input_cache = 7",
        "message InputCacheCounts",
        "message TreeCounts",
        // The pack reads a daemon with `--tierstore-endpoint` read from the frontend instead.
        "field PackReadCounts.fallback_lost = 7",
        "field PackReadCounts.fallback_damaged = 8",
        // Whether the farm places by the load the nodes measure.
        "field Welcome.placement_load = 6",
        "field Result.job_outcome = 7",
        "field Start.job = 9",
        "field Start.fence = 10",
        "message Job",
        "message JobLimits",
        "message JobOutcome",
        "message JobTaint",
        "message Label",
        "message LeaseClock",
        "message LeaseMinute",
        "message Mount",
        "value Phase.PHASE_TAINTED = 6",
    ]
    .map(str::to_owned)
    .to_vec();
    want.sort();
    assert_eq!(additions(&before(), &today()), want);
}

/// Catches a comparison that cannot fail: each kind of break, planted in a copy of today's
/// file, is named.
#[test]
fn the_comparison_names_each_kind_of_break() {
    let old = before();
    let now = today();
    let broken = |edit: fn(&mut FileDescriptorProto)| {
        let mut f = now.clone();
        edit(&mut f);
        breaks(&old, &f)
    };
    fn msg(f: &FileDescriptorProto, name: &str) -> usize {
        f.message_type
            .iter()
            .position(|m| m.name() == name)
            .unwrap_or_else(|| panic!("no {name}"))
    }

    // A field renumbered: the earlier device_id (8) is gone from its number.
    let got = broken(|f| {
        let i = msg(f, "Start");
        let field = f.message_type[i]
            .field
            .iter_mut()
            .find(|x| x.name() == "device_id")
            .expect("device_id");
        field.number = Some(11);
    });
    assert_eq!(got, ["Start.device_id removed or renumbered"]);

    // A field retyped.
    let got = broken(|f| {
        let i = msg(f, "Result");
        let field = f.message_type[i]
            .field
            .iter_mut()
            .find(|x| x.name() == "memory_kill")
            .expect("memory_kill");
        field.r#type = Some(prost_types::field_descriptor_proto::Type::Int64 as i32);
    });
    assert_eq!(got.len(), 1);
    assert!(got[0].starts_with("Result.memory_kill changed"), "{got:?}");

    // A message removed, an enum value removed, an enum value renumbered, a method removed.
    let got = broken(|f| {
        let i = msg(f, "Cancel");
        f.message_type.remove(i);
        let cancel = f.message_type.iter().position(|m| m.name() == "Cancel");
        assert_eq!(cancel, None);
    });
    assert_eq!(got, ["message Cancel removed"]);
    let got = broken(|f| {
        let e = f
            .enum_type
            .iter_mut()
            .find(|e| e.name() == "Phase")
            .expect("Phase");
        e.value.retain(|v| v.name() != "PHASE_DRAINING");
    });
    assert_eq!(got, ["Phase.PHASE_DRAINING removed or renumbered"]);
    let got = broken(|f| {
        let e = f
            .enum_type
            .iter_mut()
            .find(|e| e.name() == "Lost")
            .expect("Lost");
        e.value.iter_mut().for_each(|v| {
            if v.name() == "LOST_FENCED" {
                v.number = Some(9);
            }
        });
    });
    assert_eq!(got, ["Lost.LOST_FENCED removed or renumbered"]);
    let got = broken(|f| f.service[0].method.clear());
    assert_eq!(got, ["Worker.Session removed or changed"]);
    let got = broken(|f| f.service.clear());
    assert_eq!(got, ["service Worker removed"]);
    let got = broken(|f| {
        let e = f
            .enum_type
            .iter()
            .position(|e| e.name() == "Lost")
            .expect("Lost");
        f.enum_type.remove(e);
    });
    assert_eq!(got, ["enum Lost removed"]);

    // A oneof dropped from a message that had one.
    let got = broken(|f| {
        let i = msg(f, "DaemonMessage");
        f.message_type[i].oneof_decl.clear();
    });
    assert_eq!(got, ["message DaemonMessage oneofs changed"]);

    // An unrelated addition is not a break.
    assert_eq!(breaks(&old, &now), Vec::<String>::new());
}
