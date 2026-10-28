//! The services of a Linux host other than kbf, made from the configuration as they are
//! asked for ([`super::services`] says how they are served).

use std::path::{Path, PathBuf};
use std::rc::Rc;

use super::answer::Answer;
use super::backend::{Backend, Fault, Pending};
use super::core::Ab;
use super::linux::config_fault;
use super::request::LocalRequest;
use super::services::{Services, not_configured};
use crate::mac::channel::MacAllowlist;
use crate::request::{Channel, Version};
use crate::service::{Descriptor, ServiceName};
use crate::trust::Trust;

impl Backend for Box<dyn Backend + '_> {
    fn lock_file(&self) -> PathBuf {
        (**self).lock_file()
    }
    fn node_id(&self) -> Option<String> {
        (**self).node_id()
    }
    fn current(&self) -> Result<Option<String>, Fault> {
        (**self).current()
    }
    fn pending(&self) -> Result<Option<Pending>, Fault> {
        (**self).pending()
    }
    fn has(&self, commit: &str) -> bool {
        (**self).has(commit)
    }
    fn stage(&self, version: &Version, channel: &Channel) -> Result<String, Fault> {
        (**self).stage(version, channel)
    }
    fn switch(&self, commit: &str, window_secs: u64, now: u64) -> Result<u64, Fault> {
        (**self).switch(commit, window_secs, now)
    }
    fn confirm(&self, pending: &Pending) -> Result<(), Fault> {
        (**self).confirm(pending)
    }
    fn revert(&self, pending: &Pending) -> Result<(), Fault> {
        (**self).revert(pending)
    }
}

/// The descriptors of the services the allowlist at `path` describes.
///
/// # Errors
/// The allowlist cannot be trusted or read: a refusal with the code its words give.
pub fn describe(trust: &Trust, path: &Path) -> Result<Vec<Descriptor>, Fault> {
    let allowlist = MacAllowlist::load(trust, path).map_err(config_fault)?;
    Ok(allowlist.services().cloned().collect())
}

/// The backend of a described service.
type Make<'a> = dyn Fn(&Descriptor) -> Result<Box<dyn Backend + 'a>, Fault> + 'a;

/// The services of a Linux host, made from the configuration as they are asked for.
pub struct Hub<'a> {
    /// The descriptors the configuration has now.
    pub describe: Box<dyn Fn() -> Result<Vec<Descriptor>, Fault> + 'a>,
    /// The backend of a described service.
    pub make: Box<Make<'a>>,
    /// Where each service's `state-<service>.json` is.
    pub state_dir: PathBuf,
    pub trust: Trust,
    /// How long a release on trial has to be confirmed.
    pub window_secs: u64,
    /// Unix seconds now.
    pub clock: Rc<dyn Fn() -> u64>,
}

impl<'a> Hub<'a> {
    fn ab(&self, descriptor: &Descriptor) -> Result<Ab<Box<dyn Backend + 'a>>, Fault> {
        let clock = Rc::clone(&self.clock);
        Ok(Ab::new(
            (self.make)(descriptor)?,
            self.state_dir
                .join(format!("state-{}.json", descriptor.name.as_str())),
            self.trust.clone(),
            self.window_secs,
            Box::new(move || clock()),
        ))
    }
}

impl Services for Hub<'_> {
    fn handle(&self, service: &ServiceName, request: &LocalRequest) -> Answer {
        let found = (self.describe)().map(|all| all.into_iter().find(|d| &d.name == service));
        match found {
            Err(fault) => fault.answer(),
            Ok(None) => not_configured(service),
            Ok(Some(descriptor)) => match self.ab(&descriptor) {
                Ok(ab) => ab.handle(request),
                Err(fault) => fault.answer(),
            },
        }
    }

    fn tick(&self) -> Vec<(ServiceName, Answer)> {
        let Ok(all) = (self.describe)() else {
            return Vec::new();
        };
        all.iter()
            .filter_map(|descriptor| {
                let answer = self.ab(descriptor).ok()?.tick()?;
                Some((descriptor.name.clone(), answer))
            })
            .collect()
    }
}

/// The services of this host as the program runs them: the channels file of the local socket
/// ([`super::linux::ALLOWLIST`]) describes them, and each is laid out by
/// [`super::linux::production_for`].
pub fn production(trust: &Trust, state_dir: &Path, window_secs: u64) -> Hub<'static> {
    let (reader, path) = (trust.clone(), PathBuf::from(super::linux::ALLOWLIST));
    Hub {
        describe: Box::new(move || describe(&reader, &path)),
        make: Box::new(|descriptor| {
            make_production(descriptor)
                .map_err(|why| Fault::refused(super::answer::Code::ConfigInvalid, why))
        }),
        state_dir: state_dir.to_owned(),
        trust: trust.clone(),
        window_secs,
        clock: Rc::new(super::production::unix_now),
    }
}

/// The backend of `descriptor` as the program runs it: a container service is updated as a
/// container ([`super::frontend::production_service`]), any other as releases of a tree.
fn make_production<'a>(descriptor: &Descriptor) -> Result<Box<dyn Backend + 'a>, String> {
    if descriptor.container.is_some() {
        return super::frontend::production_service(descriptor)
            .map(|backend| Box::new(backend) as Box<dyn Backend>);
    }
    super::linux::production_for(descriptor).map(|linux| Box::new(linux) as Box<dyn Backend>)
}
