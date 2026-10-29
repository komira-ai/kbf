//! A container runtime and a frontend played by the tests: one world, so that what the
//! runtime was told and what the frontend then answers agree, and one list of events in the
//! order they happened.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use super::config::RunSpec;
use super::probe::{Probe, Reply};
use super::runtime::{Container, Runtime, RuntimeError};
use super::{FetchedImage, ImageSource, Pace};
use crate::local::backend::Fault;
use crate::request::{Channel, Version};

/// A container the world holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Made {
    pub(super) image_id: String,
    pub(super) image_ref: String,
    pub(super) running: bool,
    pub(super) restart: String,
    pub(super) args: Vec<String>,
    pub(super) draining: bool,
}

#[derive(Default)]
pub(super) struct Inner {
    /// What happened, in order: `fetch`, `load`, `set_restart:no`, `exec:...`, `stop`,
    /// `remove`, `run:<image id>`, `get:<addr><path>`, `remove_image:<id>`.
    pub(super) events: Vec<String>,
    /// Virtual time since the start.
    pub(super) now: Duration,
    pub(super) sleeps: u32,
    /// Image tags and the ids they name.
    pub(super) tags: BTreeMap<String, String>,
    pub(super) ids: BTreeSet<String>,
    /// The commit each image id holds.
    pub(super) commit_of: BTreeMap<String, String>,
    /// What the next `load` brings in: tag, id, commit.
    pub(super) will_load: Option<(String, String, String)>,
    pub(super) container: Option<Made>,
    /// The frontend of an image never gets ready.
    pub(super) never_ready: BTreeSet<String>,
    /// A container made from one of these images ends as soon as it starts.
    pub(super) crashes: BTreeSet<String>,
    /// `inspect` fails from this many calls on.
    pub(super) inspect_fails_from: Option<u32>,
    pub(super) inspects: u32,
    /// A drained container ends after this many looks (one when none).
    pub(super) ends_after: Option<u32>,
    /// A drained container never ends: it has work that does not finish.
    pub(super) never_ends: bool,
    pub(super) drain_looks: u32,
    /// Calls that fail: `load`, `set_restart`, `exec`, `stop`, `remove`, `run:<id>`,
    /// `inspect`, `image_id`.
    pub(super) fail: BTreeSet<String>,
    /// The `/readyz` status of the other frontends' APIs; no entry is a refused connection.
    pub(super) peers: BTreeMap<SocketAddr, u16>,
    /// The `/healthz` of the frontend says another commit than the image's.
    pub(super) lies_about_commit: Option<String>,
    /// Every `run` that was asked, as docker's arguments.
    pub(super) runs: Vec<Vec<String>>,
    /// The container's name and operator API when they are not the default ones.
    pub(super) serving: Option<(String, SocketAddr)>,
}

/// The world, shared by the runtime, the probe and the test.
#[derive(Clone, Default)]
pub(super) struct World(pub(super) Rc<RefCell<Inner>>);

pub(super) const API: &str = "127.0.0.1:8982";

impl World {
    pub(super) fn events(&self) -> Vec<String> {
        self.0.borrow().events.clone()
    }

    /// An image of `commit` is in the runtime under `kbf-frontend:<commit>`, with id
    /// `sha256:<name>`.
    pub(super) fn image(&self, commit: &str, name: &str) -> String {
        let id = format!("sha256:{name}");
        let mut inner = self.0.borrow_mut();
        inner
            .tags
            .insert(format!("kbf-frontend:{commit}"), id.clone());
        inner.ids.insert(id.clone());
        inner.commit_of.insert(id.clone(), commit.to_owned());
        id
    }

    /// A container that runs `image` (an id from [`World::image`]), made the way `spec` says.
    pub(super) fn container(&self, spec: &RunSpec, image: &str, commit: &str) {
        let mut inner = self.0.borrow_mut();
        inner.container = Some(Made {
            image_id: image.to_owned(),
            image_ref: format!("kbf-frontend:{commit}"),
            running: true,
            restart: spec.restart.clone(),
            args: spec.run_args(image),
            draining: false,
        });
    }

    /// The next `load` brings in the image of `commit`.
    pub(super) fn will_load(&self, commit: &str, name: &str) -> String {
        let id = format!("sha256:{name}");
        self.0.borrow_mut().will_load = Some((
            format!("kbf-frontend:{commit}"),
            id.clone(),
            commit.to_owned(),
        ));
        id
    }

    /// The container is called `name` and its operator API is `api`.
    pub(super) fn serving(&self, name: &str, api: &str) {
        self.0.borrow_mut().serving = Some((name.to_owned(), api.parse().unwrap()));
    }

    pub(super) fn made(&self) -> Option<Made> {
        self.0.borrow().container.clone()
    }

    pub(super) fn fail(&self, call: &str) {
        self.0.borrow_mut().fail.insert(call.to_owned());
    }

    pub(super) fn heal(&self, call: &str) {
        self.0.borrow_mut().fail.remove(call);
    }

    pub(super) fn elapsed(&self) -> Duration {
        self.0.borrow().now
    }

    /// The runs, as docker's arguments.
    pub(super) fn runs(&self) -> Vec<Vec<String>> {
        self.0.borrow().runs.clone()
    }

    /// The pace of the waits in this world: a look every second of virtual time.
    pub(super) fn pace(&self) -> Pace {
        let (now, sleep) = (self.clone(), self.clone());
        Pace {
            poll: Duration::from_secs(1),
            now: Box::new(move || now.0.borrow().now),
            sleep: Box::new(move |d| {
                let mut inner = sleep.0.borrow_mut();
                inner.now += d;
                inner.sleeps += 1;
                if inner.container.as_ref().is_some_and(|c| c.draining) {
                    inner.drain_looks += 1;
                    if !inner.never_ends && inner.drain_looks >= inner.ends_after.unwrap_or(1) {
                        inner.container.as_mut().unwrap().running = false;
                    }
                }
            }),
        }
    }
}

fn failure(inner: &Inner, call: &str) -> Result<(), RuntimeError> {
    if inner.fail.contains(call) {
        return Err(RuntimeError(format!("docker {call} failed")));
    }
    Ok(())
}

impl Runtime for World {
    fn load(&self, _file: &Path) -> Result<(), RuntimeError> {
        let mut inner = self.0.borrow_mut();
        inner.events.push("load".to_owned());
        failure(&inner, "load")?;
        if let Some((tag, id, commit)) = inner.will_load.take() {
            inner.tags.insert(tag, id.clone());
            inner.ids.insert(id.clone());
            inner.commit_of.insert(id, commit);
        }
        Ok(())
    }

    fn image_id(&self, reference: &str) -> Result<Option<String>, RuntimeError> {
        let inner = self.0.borrow();
        failure(&inner, "image_id")?;
        Ok(inner
            .tags
            .get(reference)
            .cloned()
            .or_else(|| inner.ids.contains(reference).then(|| reference.to_owned())))
    }

    fn remove_image(&self, id: &str) -> Result<(), RuntimeError> {
        let mut inner = self.0.borrow_mut();
        inner.events.push(format!("remove_image:{id}"));
        inner.ids.remove(id);
        Ok(())
    }

    fn container(&self, name: &str) -> Result<Option<Container>, RuntimeError> {
        let mut inner = self.0.borrow_mut();
        failure(&inner, "inspect")?;
        inner.inspects += 1;
        if inner.inspect_fails_from.is_some_and(|n| inner.inspects > n) {
            return Err(RuntimeError("docker inspect failed".to_owned()));
        }
        let expected = inner
            .serving
            .as_ref()
            .map_or("kbf-frontend", |s| s.0.as_str());
        assert_eq!(name, expected);
        Ok(inner.container.as_ref().map(|c| Container {
            image_id: c.image_id.clone(),
            image_ref: c.image_ref.clone(),
            running: c.running,
            restart: c.restart.clone(),
        }))
    }

    fn set_restart(&self, _name: &str, policy: &str) -> Result<(), RuntimeError> {
        let mut inner = self.0.borrow_mut();
        inner.events.push(format!("set_restart:{policy}"));
        failure(&inner, "set_restart")?;
        inner.container.as_mut().unwrap().restart = policy.to_owned();
        Ok(())
    }

    fn exec(&self, _name: &str, argv: &[String]) -> Result<(), RuntimeError> {
        let mut inner = self.0.borrow_mut();
        inner.events.push(format!("exec:{}", argv.join(" ")));
        failure(&inner, "exec")?;
        let c = inner.container.as_mut().unwrap();
        assert!(c.running, "a container that is not running is not drained");
        c.draining = true;
        Ok(())
    }

    fn stop(&self, _name: &str, _grace: u64) -> Result<(), RuntimeError> {
        let mut inner = self.0.borrow_mut();
        inner.events.push("stop".to_owned());
        failure(&inner, "stop")?;
        inner.container.as_mut().unwrap().running = false;
        Ok(())
    }

    fn remove(&self, _name: &str) -> Result<(), RuntimeError> {
        let mut inner = self.0.borrow_mut();
        inner.events.push("remove".to_owned());
        failure(&inner, "remove")?;
        inner.container = None;
        Ok(())
    }

    fn run(&self, spec: &RunSpec, image_id: &str) -> Result<(), RuntimeError> {
        let mut inner = self.0.borrow_mut();
        inner.events.push(format!("run:{image_id}"));
        failure(&inner, &format!("run:{image_id}"))?;
        assert!(inner.container.is_none(), "the name is in use");
        let args = spec.run_args(image_id);
        inner.runs.push(args.clone());
        let commit = inner.commit_of.get(image_id).cloned().unwrap_or_default();
        let running = !inner.crashes.contains(image_id);
        inner.container = Some(Made {
            image_id: image_id.to_owned(),
            image_ref: format!("kbf-frontend:{commit}"),
            running,
            restart: spec.restart.clone(),
            args,
            draining: false,
        });
        Ok(())
    }
}

impl Probe for World {
    fn get(&self, addr: SocketAddr, path: &str) -> Result<Reply, String> {
        let mut inner = self.0.borrow_mut();
        inner.events.push(format!("get:{addr}{path}"));
        if let Some(status) = inner.peers.get(&addr) {
            return Ok(Reply {
                status: *status,
                body: String::new(),
            });
        }
        let refused = || Err(format!("{addr}{path}: connection refused"));
        let api = inner
            .serving
            .as_ref()
            .map_or_else(|| API.parse().unwrap(), |s| s.1);
        if addr != api {
            return refused();
        }
        let Some(c) = inner.container.as_ref().filter(|c| c.running) else {
            return refused();
        };
        let commit = inner
            .lies_about_commit
            .clone()
            .or_else(|| inner.commit_of.get(&c.image_id).cloned())
            .unwrap_or_default();
        let ready = !inner.never_ready.contains(&c.image_id);
        Ok(match path {
            "/readyz" => Reply {
                status: if ready { 200 } else { 503 },
                body: String::new(),
            },
            _ => Reply {
                status: 200,
                body: format!(
                    "{{\"status\":\"alive\",\"commit\":\"{}\"}}",
                    commit.get(..12).unwrap_or(&commit)
                ),
            },
        })
    }
}

/// The source of image files: gives the release `commit`, or the fault it is told to.
pub(super) struct Source {
    pub(super) world: World,
    pub(super) answer: Rc<RefCell<Result<String, Fault>>>,
}

impl Source {
    pub(super) fn new(world: &World, commit: &str) -> Self {
        Self {
            world: world.clone(),
            answer: Rc::new(RefCell::new(Ok(commit.to_owned()))),
        }
    }
}

impl ImageSource for Source {
    fn fetch(&self, _version: &Version, _channel: &Channel) -> Result<FetchedImage, Fault> {
        self.world.0.borrow_mut().events.push("fetch".to_owned());
        self.answer
            .borrow()
            .clone()
            .map(|commit| FetchedImage::new(commit, PathBuf::from("/image.tar"), None))
    }
}
