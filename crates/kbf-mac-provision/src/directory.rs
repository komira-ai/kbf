//! The local directory as the pool items read and change it: the users and groups read
//! once (`dscl . -readall`), and the changes to many records made in one `dscl`
//! process fed its commands on standard input.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::accounts::DSCL;
use crate::items::Ctx;

/// The attributes read of each user.
const USER_KEYS: [&str; 8] = [
    "RecordName",
    "UniqueID",
    "PrimaryGroupID",
    "UserShell",
    "NFSHomeDirectory",
    "IsHidden",
    "Password",
    "AuthenticationAuthority",
];
/// The attributes read of each group.
const GROUP_KEYS: [&str; 3] = ["RecordName", "PrimaryGroupID", "GroupMembership"];

/// The prefix `dscl` puts on an attribute outside its standard set:
/// `dsAttrTypeNative:IsHidden: 1`.
const NATIVE: &str = "dsAttrTypeNative:";

/// One directory record: its name and the attributes read, each as the words `dscl`
/// printed for it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Record {
    pub(crate) name: String,
    attrs: BTreeMap<String, String>,
}

impl Record {
    /// Whether the record has attribute `attr`.
    pub(crate) fn has(&self, attr: &str) -> bool {
        self.attrs.contains_key(attr)
    }

    /// The value of `attr`, empty when the record has none.
    pub(crate) fn get(&self, attr: &str) -> &str {
        self.attrs.get(attr).map_or("", String::as_str)
    }

    /// `attr` as a number, when it is one.
    pub(crate) fn number(&self, attr: &str) -> Option<u64> {
        self.get(attr).parse().ok()
    }
}

/// The records `dscl -readall` printed: one block of `Attribute: value` lines for each,
/// the blocks ended by a line of `-`. A value that does not fit on its line goes on the
/// lines after it, each indented. A block with no `RecordName` is dropped.
pub(crate) fn parse(text: &str) -> Vec<Record> {
    let mut records = Vec::new();
    let mut attrs: BTreeMap<String, String> = BTreeMap::new();
    let mut last = String::new();
    let mut end = |attrs: &mut BTreeMap<String, String>| {
        let name = attrs
            .remove("RecordName")
            .and_then(|v| v.split_whitespace().next().map(str::to_owned));
        let attrs = std::mem::take(attrs);
        if let Some(name) = name {
            records.push(Record { name, attrs });
        }
    };
    for line in text.lines() {
        if line == "-" {
            end(&mut attrs);
        } else if line.starts_with(char::is_whitespace) {
            if let Some(value) = attrs.get_mut(&last) {
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(line.trim());
            }
        } else if let Some((key, value)) = line.strip_prefix(NATIVE).unwrap_or(line).split_once(':')
        {
            last = key.to_owned();
            attrs.insert(last.clone(), value.trim().to_owned());
        }
    }
    end(&mut attrs);
    records
}

/// Every user and group of the local directory, read once.
pub(crate) struct Directory {
    users: Vec<Record>,
    groups: Vec<Record>,
}

impl Directory {
    /// Reads `/Users` and `/Groups`: two `dscl` runs, however many records there are.
    pub(crate) fn read(c: &Ctx<'_>) -> Self {
        let all = |dir: &str, keys: &[&str]| {
            let mut args = vec![".", "-readall", dir];
            args.extend_from_slice(keys);
            parse(&c.mac.out(DSCL, &args))
        };
        Self {
            users: all("/Users", &USER_KEYS),
            groups: all("/Groups", &GROUP_KEYS),
        }
    }

    pub(crate) fn user(&self, name: &str) -> Option<&Record> {
        self.users.iter().find(|u| u.name == name)
    }

    pub(crate) fn group(&self, name: &str) -> Option<&Record> {
        self.groups.iter().find(|g| g.name == name)
    }

    /// The user other than those `ours` accepts that has uid `uid`.
    pub(crate) fn uid_holder(&self, uid: u64, ours: impl Fn(&str) -> bool) -> Option<&str> {
        self.users
            .iter()
            .find(|u| u.number("UniqueID") == Some(uid) && !ours(&u.name))
            .map(|u| u.name.as_str())
    }

    /// The group other than `me` that has gid `gid`.
    pub(crate) fn gid_holder(&self, gid: u64, me: &str) -> Option<&str> {
        self.groups
            .iter()
            .find(|g| g.number("PrimaryGroupID") == Some(gid) && g.name != me)
            .map(|g| g.name.as_str())
    }

    /// Whether `user` is in `group`: listed in its `GroupMembership`, or having `gid`
    /// (the group's fixed id) as its primary group.
    pub(crate) fn is_member(&self, user: &Record, group: &str, gid: u64) -> bool {
        user.number("PrimaryGroupID") == Some(gid)
            || self.group(group).is_some_and(|g| {
                g.get("GroupMembership")
                    .split_whitespace()
                    .any(|m| m == user.name)
            })
    }
}

/// Commands for one `dscl .` process to run in order, as it reads them on standard
/// input: `create <record> [<attribute> <value>]` and `delete <record> <attribute>`.
/// Nothing in them is quoted: no record, attribute or value holds a space.
#[derive(Debug, Default)]
pub(crate) struct Batch(String);

impl Batch {
    pub(crate) fn create(&mut self, record: &str) {
        let _ = writeln!(self.0, "create {record}");
    }

    pub(crate) fn set(&mut self, record: &str, attr: &str, value: &str) {
        let _ = writeln!(self.0, "create {record} {attr} {value}");
    }

    pub(crate) fn delete(&mut self, record: &str, attr: &str) {
        let _ = writeln!(self.0, "delete {record} {attr}");
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Runs the commands in one `dscl .` process; nothing runs when there are none. The
    /// exit status is not used: `apply` checks every item again afterwards, and says
    /// what is still wrong.
    pub(crate) fn run(&self, c: &Ctx<'_>) {
        if !self.is_empty() {
            c.mac.succeeds_with_input(DSCL, &["."], &self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Batch, parse};

    /// Catches: a value read from the wrong line or key: the `dsAttrTypeNative:` prefix
    /// kept in the key, a value on the lines after its key lost, a record's attributes
    /// run into the next record's, a record without a name kept, and the last record
    /// lost when no `-` ends it.
    #[test]
    fn readall_output_is_split_into_records() {
        let text = "\
AuthenticationAuthority: ;ShadowHash;HASHLIST:<SALTED-SHA512-PBKDF2>
NFSHomeDirectory: /opt/farm/a b
PrimaryGroupID:
 20
RecordName: farm farmadmin
UniqueID: 501
dsAttrTypeNative:IsHidden: 1
-
UniqueID: 9
-
RecordName:
 pool
GroupMembership:
 x
 y
UniqueID: 700";
        let records = parse(text);
        assert_eq!(records.len(), 2, "{records:?}");
        let (farm, pool) = (&records[0], &records[1]);
        assert_eq!(farm.name, "farm");
        assert_eq!(farm.get("UniqueID"), "501");
        assert_eq!(farm.number("UniqueID"), Some(501));
        assert_eq!(farm.get("PrimaryGroupID"), "20");
        assert_eq!(farm.get("IsHidden"), "1");
        assert_eq!(farm.get("NFSHomeDirectory"), "/opt/farm/a b");
        assert!(farm.has("AuthenticationAuthority"));
        assert_eq!(
            farm.get("AuthenticationAuthority"),
            ";ShadowHash;HASHLIST:<SALTED-SHA512-PBKDF2>"
        );
        assert!(!farm.has("Password"));
        assert_eq!(farm.get("Password"), "");
        assert_eq!(farm.number("Password"), None);
        assert_eq!(pool.name, "pool");
        assert_eq!(pool.get("GroupMembership"), "x y");
        assert_eq!(pool.get("UniqueID"), "700");
        assert!(!pool.has("RecordName"));
    }

    /// Catches: a blank read (a failed `dscl`) producing a record, and a continuation
    /// line before any key being taken for a key.
    #[test]
    fn nothing_read_is_no_records() {
        assert!(parse("").is_empty());
        assert!(parse(" orphan\nno colon here\n-\n").is_empty());
    }

    /// Catches: batch commands in a form `dscl .` does not read on standard input, or
    /// out of order, and a batch with nothing in it reported as having something.
    #[test]
    fn a_batch_is_one_command_per_line_in_the_order_queued() {
        let mut batch = Batch::default();
        assert!(batch.is_empty());
        batch.create("/Groups/u");
        batch.set("/Groups/u", "UniqueID", "7");
        batch.delete("/Groups/u", "AuthenticationAuthority");
        assert!(!batch.is_empty());
        assert_eq!(
            batch.0,
            "create /Groups/u\ncreate /Groups/u UniqueID 7\ndelete /Groups/u AuthenticationAuthority\n"
        );
    }
}
