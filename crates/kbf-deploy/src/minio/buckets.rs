//! `kbf-deploy minio buckets`: kbf's buckets on a MinIO, a policy that reaches only
//! them, and a service account key for kbf-server.
//!
//! The plan, after it checks that `<endpoint>/minio/health/live` answers 200 and that
//! `--mc` is the MinIO client:
//!
//! 1. each `--bucket` that `mc ls` does not find is made (`mc mb`);
//! 2. the policy `--policy` allows `s3:ListBucket` and `s3:GetBucketLocation` on those
//!    buckets and `s3:GetObject`, `s3:PutObject` and `s3:DeleteObject` on their
//!    objects, nothing else. It is created (`mc admin policy create`, which replaces a
//!    policy of that name) unless `mc admin policy info` shows the same statements;
//! 3. with `--key-file` and no file there: a service account named `--account` is
//!    minted under the admin user, carrying that policy inline (so its access does not
//!    change if the named policy does), and its pair is written to the file, mode 0600,
//!    as the `AWS_ACCESS_KEY_ID=` and `AWS_SECRET_ACCESS_KEY=` lines kbf-server's
//!    `--s3-env-file` reads; owned by `--key-owner` when given. The file's directory
//!    must exist, and no directory above it may hold a `.git`.
//!
//! An existing key file is kept. Its mode and owner are set again when they differ,
//! and each bucket that already exists is listed with its key, so a key that no longer
//! works is an error rather than a kept file. A re-run after an apply plans nothing.
//!
//! `--remove` (rollback) removes each bucket WITH its objects (`mc rb --force`), the
//! service account whose id the key file holds, the key file, and the policy.
//!
//! A plan with `--key-owner` changes a file another user owns, so it applies only as
//! root; without it, only as a user other than root.

use std::collections::BTreeSet;
use std::io::Write;

use serde::{Deserialize, Serialize};

use super::{Admin, KbfName, MinioError, checked, exits_0, inspect, note, query, url_encode};
use crate::config::Config;
use crate::exec::{Entry, Exec};
use crate::path::HostPath;
use crate::plan::{Plan, Planner, RunAs};
use crate::step::{Cmd, Filter, HttpUrl, Owner, Perms};

/// What `minio buckets` is asked to do.
pub struct Buckets {
    pub endpoint: HttpUrl,
    pub buckets: Vec<KbfName>,
    pub policy: KbfName,
    pub account: KbfName,
    pub key_file: Option<HostPath>,
    pub key_owner: Option<String>,
    pub mc: HostPath,
    /// `mc`'s `MC_CONFIG_DIR`: a private scratch directory, removed after the run.
    pub mc_config_dir: String,
    pub remove: bool,
}

/// The `mc` alias of the admin, and of the key in the key file.
const ADMIN: &str = "kbf";
const KEY: &str = "kbfkey";
const KEY_PERMS: Perms = Perms::new(0o600);
const KEY_LINES: &[&str] = &["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"];
const ID: &str = "/usr/bin/id";

/// How a minted pair becomes the key file.
pub const S3_ENV: Filter = Filter {
    name: "s3-env",
    apply: s3_env,
};

impl Buckets {
    pub fn run_as(&self) -> RunAs {
        if self.key_owner.is_some() {
            RunAs::Root
        } else {
            RunAs::User
        }
    }

    /// `mc <args>` as the holder of `access`/`secret`, known to `mc` as `alias`.
    fn mc(&self, alias: &str, access: &str, secret: &str, args: &[&str]) -> Cmd {
        let url = format!(
            "http://{}:{}@{}",
            url_encode(access),
            url_encode(secret),
            self.endpoint.authority()
        );
        let mut cmd = Cmd::new(self.mc.clone())
            .env(format!("MC_HOST_{alias}"), url)
            .env("MC_CONFIG_DIR", &self.mc_config_dir);
        for arg in args {
            cmd = cmd.arg(*arg);
        }
        cmd
    }
}

/// The plan for `args`, asking MinIO (through `exec`) what it has. Lines about what is
/// kept go to `out`.
pub fn plan<E: Exec, W: Write>(
    exec: &E,
    args: &Buckets,
    admin: &Admin,
    out: &mut W,
) -> Result<Plan, MinioError> {
    let live = HttpUrl::new(&format!(
        "http://{}/minio/health/live",
        args.endpoint.authority()
    ))?;
    match exec.http_status(&live) {
        Ok(200) => {}
        Ok(status) => {
            return Err(MinioError::NotLive {
                url: live.to_string(),
                why: format!("it answered {status}"),
            });
        }
        Err(e) => {
            return Err(MinioError::NotLive {
                url: live.to_string(),
                why: e.to_string(),
            });
        }
    }
    let version = checked(exec, &Cmd::new(args.mc.clone()).arg("--version"))?;
    if !String::from_utf8_lossy(&version.stdout).contains("RELEASE.") {
        return Err(MinioError::NotMc(args.mc.clone()));
    }
    let as_admin = |a: &[&str]| args.mc(ADMIN, &admin.user, &admin.password, a);
    let mut planner = Planner::new(exec);
    let mut existing = Vec::new();
    for bucket in &args.buckets {
        let target = format!("{ADMIN}/{bucket}");
        if exits_0(exec, &as_admin(&["ls", &target]))? {
            existing.push(bucket);
        } else if !args.remove {
            planner.run(as_admin(&["mb", &target]));
        }
    }
    if args.remove {
        remove(exec, args, &as_admin, &existing, &mut planner, out)?;
        return Ok(planner.finish());
    }
    for bucket in &existing {
        note(out, format_args!("bucket {bucket}: exists, kept"))?;
    }

    let policy = policy_json(&args.buckets)?;
    let info = policy_info(
        exec,
        &as_admin(&["admin", "policy", "info", ADMIN, &args.policy.0, "--json"]),
    )?;
    if info.as_ref() == Some(&policy_doc(&args.buckets)) {
        note(out, format_args!("policy {}: as wanted, kept", args.policy))?;
    } else {
        let create = as_admin(&[
            "admin",
            "policy",
            "create",
            ADMIN,
            &args.policy.0,
            "/dev/stdin",
        ]);
        planner.run(create.stdin(policy.clone()));
    }

    let Some(path) = &args.key_file else {
        note(out, format_args!("no --key-file: no service account made"))?;
        return Ok(planner.finish());
    };
    refuse_in_git(exec, path)?;
    let owner = match &args.key_owner {
        Some(name) => Some(owner_of(exec, name)?),
        None => None,
    };
    match inspect(exec, path)? {
        None => {
            let description = format!("kbf-deploy: {}", args.account);
            let add = as_admin(&[
                "admin",
                "user",
                "svcacct",
                "add",
                "--json",
                "--policy",
                "/dev/stdin",
                "--name",
                &args.account.0,
                "--description",
                &description,
                ADMIN,
                &admin.user,
            ]);
            planner.capture(add.stdin(policy), S3_ENV, path.clone(), KEY_PERMS, owner);
        }
        Some(Entry::File { bytes, .. }) => {
            let (access, secret) = key_pair(path, &bytes)?;
            planner.file(path.clone(), KEY_PERMS, owner, bytes)?;
            for bucket in &existing {
                let ls = args.mc(KEY, &access, &secret, &["ls", &format!("{KEY}/{bucket}")]);
                if !exits_0(exec, &ls)? {
                    return Err(MinioError::KeyCannotList {
                        path: path.clone(),
                        bucket: (*bucket).clone(),
                    });
                }
            }
            note(out, format_args!("key {path}: exists, kept"))?;
        }
        Some(_) => return Err(MinioError::NotAFile(path.clone())),
    }
    Ok(planner.finish())
}

fn remove<E: Exec, W: Write>(
    exec: &E,
    args: &Buckets,
    as_admin: &dyn Fn(&[&str]) -> Cmd,
    existing: &[&KbfName],
    planner: &mut Planner<'_, E>,
    out: &mut W,
) -> Result<(), MinioError> {
    for bucket in &args.buckets {
        if existing.contains(&bucket) {
            planner.run(as_admin(&["rb", "--force", &format!("{ADMIN}/{bucket}")]));
        } else {
            note(out, format_args!("bucket {bucket}: absent"))?;
        }
    }
    if let Some(path) = &args.key_file {
        if let Some(Entry::File { bytes, .. }) = inspect(exec, path)? {
            let (access, _) = key_pair(path, &bytes)?;
            planner.run(as_admin(&[
                "admin", "user", "svcacct", "rm", ADMIN, &access,
            ]));
            planner.absent(path.clone())?;
        } else {
            note(out, format_args!("key {path}: absent"))?;
        }
    }
    let info = as_admin(&["admin", "policy", "info", ADMIN, &args.policy.0]);
    if exits_0(exec, &info)? {
        planner.run(as_admin(&[
            "admin",
            "policy",
            "remove",
            ADMIN,
            &args.policy.0,
        ]));
    } else {
        note(out, format_args!("policy {}: absent", args.policy))?;
    }
    Ok(())
}

/// One statement of an S3 policy. The sets make two policies that list the same
/// actions and resources in another order equal, as MinIO treats them.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Statement {
    #[serde(rename = "Effect")]
    effect: String,
    #[serde(rename = "Action")]
    action: BTreeSet<String>,
    #[serde(rename = "Resource")]
    resource: BTreeSet<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PolicyDoc {
    #[serde(rename = "Version")]
    version: String,
    #[serde(rename = "Statement")]
    statement: Vec<Statement>,
}

fn policy_doc(buckets: &[KbfName]) -> PolicyDoc {
    let allow = |action: &[&str], suffix: &str| Statement {
        effect: "Allow".to_owned(),
        action: action.iter().map(|a| (*a).to_owned()).collect(),
        resource: buckets
            .iter()
            .map(|b| format!("arn:aws:s3:::{b}{suffix}"))
            .collect(),
    };
    PolicyDoc {
        version: "2012-10-17".to_owned(),
        statement: vec![
            allow(&["s3:ListBucket", "s3:GetBucketLocation"], ""),
            allow(&["s3:GetObject", "s3:PutObject", "s3:DeleteObject"], "/*"),
        ],
    }
}

fn policy_json(buckets: &[KbfName]) -> Result<Vec<u8>, MinioError> {
    serde_json::to_vec(&policy_doc(buckets)).map_err(MinioError::Serialize)
}

/// What `mc admin policy info --json` says the policy is; `None` when there is no such
/// policy (`mc` exits 1 and prints an error) or its answer is not one this kit reads:
/// the policy is then created again.
fn policy_info<E: Exec>(exec: &E, cmd: &Cmd) -> Result<Option<PolicyDoc>, MinioError> {
    #[derive(Deserialize)]
    struct Info {
        #[serde(rename = "policyInfo")]
        policy_info: Held,
    }
    #[derive(Deserialize)]
    struct Held {
        #[serde(rename = "Policy")]
        policy: PolicyDoc,
    }
    let out = query(exec, cmd)?;
    let info = serde_json::from_slice::<Info>(&out.stdout).ok();
    Ok(info.map(|i| i.policy_info.policy))
}

/// The access key id and secret key in a key file.
fn key_pair(path: &HostPath, bytes: &[u8]) -> Result<(String, String), MinioError> {
    let bad = |source| MinioError::KeyFile {
        path: path.clone(),
        source,
    };
    let config = Config::parse(&String::from_utf8_lossy(bytes), KEY_LINES).map_err(bad)?;
    let access = config.require(KEY_LINES[0]).map_err(bad)?.to_owned();
    let secret = config.require(KEY_LINES[1]).map_err(bad)?.to_owned();
    Ok((access, secret))
}

/// The key file's lines, from `mc admin user svcacct add --json`'s output. The output
/// holds the secret, so a refusal never quotes it.
fn s3_env(stdout: &[u8]) -> Result<Vec<u8>, String> {
    #[derive(Deserialize)]
    struct Minted {
        #[serde(rename = "accessKey")]
        access: String,
        #[serde(rename = "secretKey")]
        secret: String,
    }
    const REFUSAL: &str = "mc printed no accessKey and secretKey: check `mc admin user svcacct ls`";
    let minted: Minted = serde_json::from_slice(stdout).map_err(|_| REFUSAL.to_owned())?;
    if !(plain(&minted.access) && plain(&minted.secret)) {
        return Err(REFUSAL.to_owned());
    }
    Ok(format!(
        "AWS_ACCESS_KEY_ID={}\nAWS_SECRET_ACCESS_KEY={}\n",
        minted.access, minted.secret
    )
    .into_bytes())
}

/// A value a `KEY=VALUE` line carries whole: printable ASCII, no space.
fn plain(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_graphic())
}

/// Refuses `path` when a directory above it holds a `.git`: a key file in a checkout
/// is one `git add` away from a commit.
fn refuse_in_git<E: Exec>(exec: &E, path: &HostPath) -> Result<(), MinioError> {
    let s = path.as_str();
    for (i, _) in s.match_indices('/') {
        let git = HostPath::new(&format!("{}/.git", &s[..i]))?;
        if inspect(exec, &git)?.is_some() {
            return Err(MinioError::InGitTree {
                path: path.clone(),
                git,
            });
        }
    }
    Ok(())
}

/// The uid and primary gid of the user `name`, as `id` reports them.
fn owner_of<E: Exec>(exec: &E, name: &str) -> Result<Owner, MinioError> {
    Ok(Owner {
        uid: id_of(exec, "-u", name)?,
        gid: id_of(exec, "-g", name)?,
    })
}

fn id_of<E: Exec>(exec: &E, flag: &str, name: &str) -> Result<u32, MinioError> {
    let cmd = Cmd::new(HostPath::new(ID)?).arg(flag).arg("--").arg(name);
    let out = query(exec, &cmd)?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|_| MinioError::NoUser(name.to_owned()))
}

#[cfg(test)]
#[path = "buckets_tests.rs"]
mod tests;
