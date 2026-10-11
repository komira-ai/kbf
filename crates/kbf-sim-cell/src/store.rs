//! The fake object store: it keeps every object it is given and loses none (the
//! skeleton has no loss fault yet).

use std::collections::BTreeMap;

use kbf_meta::ObjectId;
use kbf_sim::{Event, NodeInput, Output};
use kbf_types::{Digest, Effect, StateMachine};

use crate::Msg;

/// The object store node.
#[derive(Debug, Default)]
pub struct ObjectStore {
    objects: BTreeMap<ObjectId, Digest>,
    out: Vec<Output<Msg>>,
}

impl ObjectStore {
    /// Whether some object holds `digest`'s bytes, readable now.
    #[must_use]
    pub fn holds(&self, digest: &Digest) -> bool {
        self.objects.values().any(|d| d == digest)
    }

    pub(crate) fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        std::mem::take(&mut self.out)
    }
}

impl StateMachine for ObjectStore {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        if let Event::Message {
            from,
            msg: Msg::Put { object, digest },
        } = input.event
        {
            self.objects.insert(object, digest);
            let msg = Msg::PutDone { object, digest };
            self.out.push(Output::Send { to: from, msg });
        }
        Vec::new()
    }
}
