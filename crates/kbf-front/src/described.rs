//! What an Execute call says its action is, carried in the [`Submission`] to the server,
//! whose `action usage:` line is keyed and labelled by it (kbf-server `action_usage`).
//!
//! It is the one thing the front adds for that line: the `Command`'s argv and output
//! paths, moved out of the `Command` the front decodes anyway (no copy, no extra CAS
//! read), and the label the call's `RequestMetadata` names. The server turns them into
//! a short key once, as the operation is submitted, and drops them.
//!
//! [`Submission`]: crate::Submission

use kbf_proto::reapi::Command;

use crate::recorder::ActionLabel;
use crate::request_metadata::CallMetadata;

/// An action's argv, output paths and client label, as the Execute call sent them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Described {
    /// The `Command`'s `arguments`, argv\[0\] first.
    pub arguments: Vec<String>,
    /// Every output path the `Command` names, as sent: its `output_paths`, then its
    /// `output_files` and `output_directories` (a client may send one path in both of
    /// those, buck2's compatibility mode does). Unsorted, maybe repeated.
    pub outputs: Vec<String>,
    /// The mnemonic and target the call's `RequestMetadata` names, whether or not the
    /// call is recorded.
    pub label: ActionLabel,
}

impl Described {
    /// The argv and output paths of `command`, moved out of it; no label.
    #[must_use]
    #[allow(deprecated)] // `output_files` and `output_directories`: REAPI before 2.1 sends only these.
    pub fn of(command: Command) -> Self {
        let Command {
            arguments,
            mut output_paths,
            output_files,
            output_directories,
            ..
        } = command;
        output_paths.extend(output_files);
        output_paths.extend(output_directories);
        Self {
            arguments,
            outputs: output_paths,
            label: ActionLabel::default(),
        }
    }
}

impl ActionLabel {
    /// The mnemonic and target `metadata` names (capped as its `Text` fields are);
    /// `None` for one it left empty.
    #[must_use]
    pub fn sent(metadata: &CallMetadata) -> Self {
        let value = |t: &crate::Text| (!t.value.is_empty()).then(|| t.value.clone());
        Self {
            mnemonic: value(&metadata.action_mnemonic),
            target: value(&metadata.target_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Text;

    /// Catches: one of the three output fields left out (an older client's outputs,
    /// or a newer one's, would not key the line), and argv reordered or dropped.
    #[test]
    #[allow(deprecated)]
    fn every_output_field_is_kept_and_argv_as_sent() {
        let command = Command {
            arguments: vec!["/usr/bin/env".into(), "tool".into()],
            output_paths: vec!["p".into()],
            output_files: vec!["f".into()],
            output_directories: vec!["d".into(), "f".into()],
            ..Command::default()
        };
        let described = Described::of(command);
        assert_eq!(described.arguments, ["/usr/bin/env", "tool"]);
        assert_eq!(described.outputs, ["p", "f", "d", "f"]);
        assert_eq!(described.label, ActionLabel::default());
    }

    /// Catches: a label read from the wrong field, or an empty field taken as a value.
    #[test]
    fn a_label_is_what_the_metadata_names() {
        let text = |v: &str| Text {
            value: v.to_owned(),
            truncated: false,
        };
        let metadata = CallMetadata {
            action_mnemonic: text("CxxCompile"),
            target_id: text("//lib:foo"),
            action_id: text("not-a-label"),
            ..CallMetadata::default()
        };
        let label = ActionLabel::sent(&metadata);
        assert_eq!(label.mnemonic.as_deref(), Some("CxxCompile"));
        assert_eq!(label.target.as_deref(), Some("//lib:foo"));
        assert_eq!(
            ActionLabel::sent(&CallMetadata::default()),
            ActionLabel::default()
        );
    }
}
