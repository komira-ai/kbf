//! Not written yet: the stub the tests of [`Recording`] are written against.
#![cfg_attr(not(test), allow(dead_code))]

use kbf_front::{ExecuteCall, Finished, Recorder};
use kbf_invocation::Run;
use kbf_sched::Requeue;
use kbf_types::{Answer, Digest, LeaseGrant, LeaseId, OperationId, Refusal, StartLease};

pub(crate) struct Recording;

impl Recording {
    pub(crate) fn new() -> Self {
        todo!()
    }

    pub(crate) fn tell(&mut self, _recorder: Recorder) {
        todo!()
    }

    pub(crate) fn submit(
        &mut self,
        _call: ExecuteCall,
        _name: &str,
        _operation: OperationId,
        _joined: bool,
    ) {
        todo!()
    }

    pub(crate) fn is_recorded(&self, _operation: OperationId) -> bool {
        todo!()
    }

    pub(crate) fn granted(&self, _grant: &LeaseGrant) {
        todo!()
    }

    pub(crate) fn started(&self, _start: &StartLease) {
        todo!()
    }

    pub(crate) fn requeued(&self, _requeue: &Requeue) {
        todo!()
    }

    pub(crate) fn ran(&self, _operation: OperationId, _lease: LeaseId, _run: Run) {
        todo!()
    }

    pub(crate) fn finished(
        &mut self,
        _answer: &Answer,
        _finished: &Finished,
        _stored: Option<Digest>,
        _reason: Option<String>,
    ) {
        todo!()
    }

    pub(crate) fn followed(&mut self, _operation: OperationId, _finished: &Finished) {
        todo!()
    }

    pub(crate) fn refused(&mut self, _refusal: &Refusal) {
        todo!()
    }
}
