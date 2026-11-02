//! A host in memory for tests: a file tree, and programs that answer only as told.
//!
//! The runner is fail closed. [`FakeHost::run`] records every command line, then answers it
//! from a reply registered for exactly that line, or for a prefix of it, and fails with an
//! error for any other: a program a test did not expect is never "run", and never silently
//! succeeds. The account lookups of `root` are answered from the start.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::io;

use crate::{Cmd, GETENT, Host, Kind, Meta, Output};

/// A file or directory in the fake tree. `Debug` prints the length of the content, not the
/// content: a failed assertion on a tree that holds a secret does not print it.
#[derive(Clone, PartialEq, Eq)]
pub struct Node {
    /// File or directory.
    pub kind: Kind,
    /// The permission bits.
    pub mode: u32,
    /// The owning user.
    pub uid: u32,
    /// The owning group.
    pub gid: u32,
    /// The content (empty for a directory).
    pub bytes: Vec<u8>,
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("kind", &self.kind)
            .field("mode", &format_args!("{:04o}", self.mode))
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .field("bytes", &format_args!("{} bytes", self.bytes.len()))
            .finish()
    }
}

#[derive(Default)]
struct State {
    nodes: BTreeMap<String, Node>,
    exact: Vec<(Vec<String>, Output)>,
    prefixes: Vec<(Vec<String>, Output)>,
    calls: Vec<Vec<String>>,
    unreadable: Vec<String>,
    unstattable: Vec<String>,
    effects: Vec<(Vec<String>, String, Node)>,
}

/// The fake host.
pub struct FakeHost {
    euid: u32,
    state: RefCell<State>,
}

fn words(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|w| (*w).to_owned()).collect()
}

fn output(code: i32, stdout: &str) -> Output {
    Output {
        code,
        stdout: stdout.to_owned(),
        stderr: String::new(),
    }
}

impl FakeHost {
    /// A host whose effective user is `euid`, with an empty tree.
    pub fn new(euid: u32) -> FakeHost {
        let host = FakeHost {
            euid,
            state: RefCell::new(State::default()),
        };
        host.reply(
            &[GETENT, "passwd", "root"],
            0,
            "root:x:0:0:root:/root:/bin/sh\n",
        );
        host.reply(&[GETENT, "group", "root"], 0, "root:x:0:\n");
        host
    }

    /// Makes `argv` (the program first) answer with `code` and `stdout`, replacing an
    /// earlier reply for the same line.
    pub fn reply(&self, argv: &[&str], code: i32, stdout: &str) {
        let mut state = self.state.borrow_mut();
        state.exact.retain(|(a, _)| *a != words(argv));
        state.exact.push((words(argv), output(code, stdout)));
    }

    /// Makes every line that starts with `argv` answer with `code` and `stdout`, unless a
    /// reply for the whole line is registered.
    pub fn reply_prefix(&self, argv: &[&str], code: i32, stdout: &str) {
        let mut state = self.state.borrow_mut();
        state.prefixes.retain(|(a, _)| *a != words(argv));
        state.prefixes.push((words(argv), output(code, stdout)));
    }

    /// Registers the user `name` with `id` as its uid and gid, and its group.
    pub fn add_user(&self, name: &str, id: u32) {
        let passwd = format!("{name}:x:{id}:{id}::/nonexistent:/usr/sbin/nologin\n");
        self.reply(&[GETENT, "passwd", name], 0, &passwd);
        self.reply(&[GETENT, "group", name], 0, &format!("{name}:x:{id}:\n"));
    }

    /// Makes the command `argv`, when it runs, leave a file at `path` (as a program that
    /// writes one does).
    pub fn effect(&self, argv: &[&str], path: &str, mode: u32, owner: (u32, u32), bytes: &[u8]) {
        let node = Node {
            kind: Kind::File,
            mode,
            uid: owner.0,
            gid: owner.1,
            bytes: bytes.to_vec(),
        };
        let mut state = self.state.borrow_mut();
        state.effects.push((words(argv), path.to_owned(), node));
    }

    /// A symbolic link at `path` to the absolute path `target`.
    pub fn put_symlink(&self, path: &str, target: &str) {
        self.put(path, Kind::Other, 0o777, (0, 0), target.as_bytes());
    }

    /// A symbolic link at `path` to somewhere that does not matter.
    pub fn put_link(&self, path: &str) {
        self.put_symlink(path, "/link-target");
    }

    /// A file at `path`, replacing what is there.
    pub fn put_file(&self, path: &str, mode: u32, owner: (u32, u32), bytes: &[u8]) {
        self.put(path, Kind::File, mode, owner, bytes);
    }

    /// A directory at `path`, replacing what is there.
    pub fn put_dir(&self, path: &str, mode: u32, owner: (u32, u32)) {
        self.put(path, Kind::Dir, mode, owner, b"");
    }

    fn put(&self, path: &str, kind: Kind, mode: u32, owner: (u32, u32), bytes: &[u8]) {
        let node = Node {
            kind,
            mode,
            uid: owner.0,
            gid: owner.1,
            bytes: bytes.to_vec(),
        };
        self.state.borrow_mut().nodes.insert(path.to_owned(), node);
    }

    /// Makes reading `path` fail with "permission denied" (it is still there).
    pub fn deny_read(&self, path: &str) {
        self.state.borrow_mut().unreadable.push(path.to_owned());
    }

    /// Makes `stat` of `path` fail with "permission denied".
    pub fn deny_stat(&self, path: &str) {
        self.state.borrow_mut().unstattable.push(path.to_owned());
    }

    /// The node at `path`.
    pub fn node(&self, path: &str) -> Option<Node> {
        self.state.borrow().nodes.get(path).cloned()
    }

    /// The paths below the root, sorted.
    pub fn paths(&self) -> Vec<String> {
        self.state.borrow().nodes.keys().cloned().collect()
    }

    /// Every command line run so far (program first), in order.
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.state.borrow().calls.clone()
    }

    /// Forgets the commands run so far.
    pub fn clear_calls(&self) {
        self.state.borrow_mut().calls.clear();
    }
}

impl Host for FakeHost {
    fn euid(&self) -> u32 {
        self.euid
    }

    fn egid(&self) -> u32 {
        self.euid
    }

    fn canonical(&self, path: &str) -> io::Result<String> {
        // The components still to walk, the next one last; targets are absolute.
        let mut todo: Vec<String> = path
            .split('/')
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
            .collect();
        todo.reverse();
        let mut done: Vec<String> = Vec::new();
        let mut hops = 0;
        while let Some(component) = todo.pop() {
            done.push(component);
            let here = format!("/{}", done.join("/"));
            let link = self.node(&here).filter(|n| n.kind == Kind::Other);
            if let Some(link) = link {
                hops += 1;
                if hops > 40 {
                    return Err(io::Error::other("fake host: too many symbolic links"));
                }
                done.clear();
                let target = String::from_utf8_lossy(&link.bytes).into_owned();
                todo.extend(
                    target
                        .split('/')
                        .filter(|c| !c.is_empty())
                        .rev()
                        .map(str::to_owned),
                );
            }
        }
        Ok(format!("/{}", done.join("/")))
    }

    fn stat(&self, path: &str) -> io::Result<Option<Meta>> {
        if self.state.borrow().unstattable.iter().any(|p| p == path) {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        let node = self.node(path);
        Ok(node.map(|n| Meta {
            kind: n.kind,
            mode: n.mode,
            uid: n.uid,
            gid: n.gid,
            size: n.bytes.len() as u64,
        }))
    }

    fn read(&self, path: &str) -> io::Result<Option<Vec<u8>>> {
        self.read_limited(path, u64::MAX)
    }

    fn read_limited(&self, path: &str, limit: u64) -> io::Result<Option<Vec<u8>>> {
        let denied = self.state.borrow().unreadable.iter().any(|p| p == path);
        match (denied, self.node(path)) {
            (true, _) => Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            (false, Some(n)) if n.kind == Kind::Other => {
                Err(io::Error::other("fake host: refused: a symbolic link"))
            }
            (false, Some(n)) if n.bytes.len() as u64 > limit => {
                Err(io::Error::from(io::ErrorKind::FileTooLarge))
            }
            (false, Some(n)) => Ok(Some(n.bytes)),
            (false, None) => Ok(None),
        }
    }

    fn write(&self, path: &str, mode: u32, owner: (u32, u32), bytes: &[u8]) -> io::Result<()> {
        self.put_file(path, mode, owner, bytes);
        Ok(())
    }

    fn mkdir(&self, path: &str, mode: u32, owner: (u32, u32)) -> io::Result<()> {
        for (i, _) in path.match_indices('/').skip(1) {
            let above = &path[..i];
            let missing = self.node(above).is_none();
            if missing {
                self.put_dir(above, 0o755, (0, 0));
            }
        }
        self.put_dir(path, mode, owner);
        Ok(())
    }

    fn remove(&self, path: &str) -> io::Result<()> {
        self.state.borrow_mut().nodes.remove(path);
        Ok(())
    }

    fn run(&self, cmd: &Cmd) -> io::Result<Output> {
        let argv = cmd.argv();
        let (reply, effects) = {
            let mut state = self.state.borrow_mut();
            state.calls.push(argv.clone());
            let exact = state.exact.iter().find(|(a, _)| *a == argv);
            let prefix = state.prefixes.iter().find(|(a, _)| argv.starts_with(a));
            let reply = exact.or(prefix).map(|(_, out)| out.clone());
            let effects: Vec<_> = state
                .effects
                .iter()
                .filter(|(a, _, _)| *a == argv)
                .cloned()
                .collect();
            (reply, effects)
        };
        let reply = reply.ok_or_else(|| {
            io::Error::other(format!("fake host: unexpected command {}", cmd.line()))
        })?;
        for (_, path, node) in effects {
            self.state.borrow_mut().nodes.insert(path, node);
        }
        Ok(reply)
    }
}
