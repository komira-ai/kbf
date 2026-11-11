//! A host in memory for the tests of the steps: it answers as it is told and logs every call.

use std::cell::RefCell;
use std::path::Path;

use crate::host::{Check, Host, HostError, UnitState};

pub struct MemHost {
    pub state: RefCell<UnitState>,
    pub version: RefCell<String>,
    /// What the package manager has installed: a start moves `version` here.
    pub installs: RefCell<String>,
    /// The state a started unit shows.
    pub after_start: RefCell<UnitState>,
    pub check: RefCell<Result<Check, HostError>>,
    /// The verbs (`stop`, `start`, `restart`, `state`, `version`) that fail.
    pub failing: RefCell<Vec<&'static str>>,
    pub calls: RefCell<Vec<String>>,
}

impl MemHost {
    /// A host whose unit is in `state` and runs `version`, and which installs `installs`.
    pub fn new(state: UnitState, version: &str, installs: &str) -> MemHost {
        MemHost {
            state: RefCell::new(state),
            version: RefCell::new(version.to_owned()),
            installs: RefCell::new(installs.to_owned()),
            after_start: RefCell::new(UnitState::Active),
            check: RefCell::new(Ok(Check::Ready)),
            failing: RefCell::new(Vec::new()),
            calls: RefCell::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    pub fn clear_calls(&self) {
        self.calls.borrow_mut().clear();
    }

    pub fn fail(&self, verb: &'static str) {
        self.failing.borrow_mut().push(verb);
    }

    fn call(&self, verb: &'static str) -> Result<(), HostError> {
        self.calls.borrow_mut().push(verb.to_owned());
        if self.failing.borrow().contains(&verb) {
            Err(HostError(format!("{verb} failed")))
        } else {
            Ok(())
        }
    }
}

impl Host for MemHost {
    fn check(&self, kbf: &Path, mode: &str) -> Result<Check, HostError> {
        self.calls
            .borrow_mut()
            .push(format!("check {} {mode}", kbf.display()));
        self.check.borrow().clone()
    }

    fn state(&self, _unit: &str) -> Result<UnitState, HostError> {
        self.call("state")?;
        Ok(self.state.borrow().clone())
    }

    fn stop(&self, _unit: &str) -> Result<(), HostError> {
        self.call("stop")?;
        *self.state.borrow_mut() = UnitState::Inactive;
        Ok(())
    }

    fn start(&self, _unit: &str) -> Result<(), HostError> {
        self.call("start")?;
        *self.version.borrow_mut() = self.installs.borrow().clone();
        *self.state.borrow_mut() = self.after_start.borrow().clone();
        Ok(())
    }

    fn restart(&self, _unit: &str) -> Result<(), HostError> {
        self.call("restart")?;
        *self.state.borrow_mut() = UnitState::Active;
        Ok(())
    }

    fn installed_version(&self) -> Result<String, HostError> {
        self.call("version")?;
        Ok(self.version.borrow().clone())
    }
}
