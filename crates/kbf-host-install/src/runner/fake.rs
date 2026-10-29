//! A runner for tests: it records every argument vector and answers from a script.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;

use super::{Cmd, Exec, Runner};

/// A look at the host as a command is about to run.
type Hook = Box<dyn Fn(&Cmd)>;

/// Replies in order, one per call; a call past the script is an error the test shows.
#[derive(Default)]
pub struct FakeRunner {
    replies: RefCell<VecDeque<io::Result<Exec>>>,
    calls: RefCell<Vec<Cmd>>,
    before: Option<Hook>,
}

impl FakeRunner {
    /// A runner that will give `replies`, one per call.
    pub fn new(replies: Vec<io::Result<Exec>>) -> Self {
        Self {
            replies: RefCell::new(replies.into()),
            calls: RefCell::default(),
            before: None,
        }
    }

    /// Run `hook` at the start of every call, before the reply: a test looks at the host
    /// while a command is about to run.
    #[must_use]
    pub fn before_each(mut self, hook: impl Fn(&Cmd) + 'static) -> Self {
        self.before = Some(Box::new(hook));
        self
    }

    /// Every command run so far.
    pub fn calls(&self) -> Vec<Cmd> {
        self.calls.borrow().clone()
    }
}

/// An exit with `code` and these outputs.
pub fn exec(code: i32, stdout: &str, stderr: &str) -> io::Result<Exec> {
    Ok(Exec {
        code: Some(code),
        signal: None,
        stdout: stdout.into(),
        stderr: stderr.into(),
    })
}

/// A successful exit with this stdout.
pub fn ok(stdout: &str) -> io::Result<Exec> {
    exec(0, stdout, "")
}

impl Runner for FakeRunner {
    fn run(&self, cmd: &Cmd) -> io::Result<Exec> {
        if let Some(hook) = &self.before {
            hook(cmd);
        }
        self.calls.borrow_mut().push(cmd.clone());
        self.replies
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Err(io::Error::other(format!("unscripted call: {cmd:?}"))))
    }
}
