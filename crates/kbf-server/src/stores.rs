//! Where the server keeps what it holds: the two store URIs (`--cold-store`,
//! `--hot-store`), the metadata directory, and how the flags resolve into them.
//!
//! - **Cold store** (`--cold-store`, required): the durable object store, where every
//!   blob ends up. An S3 bucket, or a directory (`kbf_objstore::FsStore`) on a disk the
//!   server's host keeps.
//! - **Hot store** (`--hot-store`, optional): the fast tier in front of it. Uploads
//!   land there and are flushed to the cold store in the background; reads are served
//!   from it while a blob is still there (the write tier, `kbf_tier`). Without one,
//!   uploads go straight to the cold store.
//! - **Metadata log**: the index of what the object store holds (digest to segment
//!   object and offset) and the action cache, a single-voter Raft log. It lives in
//!   `<hot store directory>/meta` when the hot store is a directory, else in
//!   `--meta-dir`; an explicit `--meta-dir` wins over the hot store's directory.
//!
//! A store URI is `file:///absolute/path` or `s3://bucket[/prefix][?query]`; for a storage
//! node's `--cold-store` also the word `registered`, which names no store at all: the node
//! takes the one the frontend registers. A memory
//! store is a `file://` path on a tmpfs; there is no `memory:` scheme. An S3 URI's query
//! carries what is not the bucket: `endpoint=http://host:port` (required),
//! `region=` (default `us-east-1`), `conditional-put=true` (the store refuses to
//! overwrite a key, as MinIO and RustFS do) and `min-put-rate=` (the slowest bytes a
//! second a PUT's body may reach the store at; default 1 MiB/s). The key pair is the
//! standard `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` of the environment.
//!
//! Sizing is not configuration: a hot store directory's budget is a share of its
//! filesystem's free space plus the bytes the tier already holds, less a floor
//! (`kbf_threads::disk`), at most `--hot-store-max-bytes`.

use std::fmt;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use kbf_meta::TierId;
use kbf_objstore::{Floor, KeyError};
use kbf_threads::disk::FreeSpace;
use percent_encoding::percent_decode_str;

/// What a prefix is percent-encoded with in a URI's path: everything but letters, digits
/// and `/ - _ . ~` (the characters a key prefix may hold).
const PATH: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// The key prefix of an `s3://bucket` URI with no path.
pub const DEFAULT_S3_PREFIX: &str = "kbf/";

/// The key prefix root under a `file://` cold store: objects go under `kbf/<log id>/`.
pub const FILE_PREFIX_ROOT: &str = "kbf/";

/// The free space a `file://` cold store's filesystem must keep: `kbf_threads::disk`'s
/// `keep_free`, 5% of its size and at least 1 GiB, as for every directory that evicts
/// to a floor. A write that would leave less is refused (the upload is answered
/// UNAVAILABLE) and wakes the evictor, which forgets the least recently used objects
/// and deletes their files while less than twice the floor is free.
pub const COLD_FILE_FLOOR: Floor = Floor::KeepFree;

/// The share of the hot store filesystem's free space (less the floor) its budget is.
pub const HOT_STORE_SHARE_PERCENT: u64 = 80;

/// The slowest request body rate an S3 URI gets without `min-put-rate`: 1 MiB/s.
pub(crate) const DEFAULT_MIN_PUT_RATE: NonZeroU64 = NonZeroU64::new(1 << 20).expect("1 MiB");

/// The signing region an S3 URI gets without `region`.
pub(crate) const DEFAULT_REGION: &str = "us-east-1";

/// `region`, or [`DEFAULT_REGION`] when it is empty: the region a store is verified and
/// opened with, whoever reads the registration.
pub(crate) fn region_or_default(region: &str) -> String {
    if region.is_empty() {
        DEFAULT_REGION.to_owned()
    } else {
        region.to_owned()
    }
}

/// The word `--cold-store` takes for "the store the frontend registers".
pub const REGISTERED: &str = "registered";

/// A store the server can be pointed at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreUri {
    /// A directory on a local disk (or a tmpfs, for a store in memory).
    File(PathBuf),
    /// An S3 bucket.
    S3(S3Uri),
    /// No store of its own: the one the frontend's registration names, which a storage
    /// node takes at its start ([`crate::cold_apply`]). Only `--cold-store` of a storage
    /// node takes it.
    Registered,
}

/// An `s3://` store URI, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Uri {
    /// The bucket.
    pub bucket: String,
    /// The key prefix, ending in `/` ([`DEFAULT_S3_PREFIX`] when the URI has no path).
    pub prefix: String,
    /// `http://host[:port]`.
    pub endpoint: String,
    /// The signing region.
    pub region: String,
    /// The store refuses to overwrite a key.
    pub conditional_put: bool,
    /// The slowest rate, in bytes a second, a request body may reach the store at.
    pub min_put_rate: NonZeroU64,
}

/// Why a store URI is refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreUriError {
    /// No `scheme://`.
    #[error(
        "{0:?} is not a store URI: use file:///absolute/path or s3://bucket/prefix?endpoint=http://host:port"
    )]
    NoScheme(String),
    /// A scheme other than `file` and `s3`.
    #[error("unknown scheme {0:?}: use file:///absolute/path or s3://bucket/prefix?endpoint=...")]
    UnknownScheme(String),
    /// `memory://`, which the server does not take.
    #[error(
        "there is no memory: store; point at a file:// directory on a tmpfs for a store in memory"
    )]
    Memory,
    /// A `file://` URI that does not name an absolute path.
    #[error("file URI {0:?}: write file:///absolute/path (no host, an absolute path)")]
    FilePath(String),
    /// A `file://` URI with a query or a fragment.
    #[error("file URI {0:?}: no ? or # (percent-encode them in a path as %3F and %23)")]
    FileQuery(String),
    /// An `s3://` URI without a bucket.
    #[error("s3 URI {0:?}: no bucket (s3://bucket/prefix?endpoint=http://host:port)")]
    NoBucket(String),
    /// An `s3://` URI with a fragment.
    #[error("s3 URI {0:?}: no # fragment")]
    S3Fragment(String),
    /// An `s3://` URI without `endpoint`.
    #[error("s3 URI {0:?}: the query needs endpoint=http://host:port")]
    NoEndpoint(String),
    /// A query key the URI does not take.
    #[error("s3 URI query {key:?}: use endpoint, region, conditional-put or min-put-rate")]
    UnknownQuery {
        /// The refused key.
        key: String,
    },
    /// A query value that is not one.
    #[error("s3 URI query {key}={value:?}: {why}")]
    BadValue {
        /// The key.
        key: &'static str,
        /// The refused value.
        value: String,
        /// What it should be.
        why: &'static str,
    },
    /// A key given twice.
    #[error("s3 URI query {0:?} is given twice")]
    Repeated(&'static str),
    /// The prefix is not a valid key prefix.
    #[error("s3 URI prefix {prefix:?}: {source}")]
    Prefix {
        /// The refused prefix.
        prefix: String,
        /// Why.
        #[source]
        source: KeyError,
    },
}

impl FromStr for StoreUri {
    type Err = StoreUriError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text == REGISTERED {
            return Ok(Self::Registered);
        }
        let (scheme, rest) = text
            .split_once("://")
            .ok_or_else(|| StoreUriError::NoScheme(text.to_owned()))?;
        match scheme.to_ascii_lowercase().as_str() {
            "file" => file(text, rest).map(Self::File),
            "s3" => S3Uri::parse(text, rest).map(Self::S3),
            "memory" => Err(StoreUriError::Memory),
            _ => Err(StoreUriError::UnknownScheme(scheme.to_owned())),
        }
    }
}

fn file(text: &str, rest: &str) -> Result<PathBuf, StoreUriError> {
    if rest.contains(['?', '#']) {
        return Err(StoreUriError::FileQuery(text.to_owned()));
    }
    let path = PathBuf::from(percent_decode_str(rest).decode_utf8_lossy().into_owned());
    // `file://host/path` leaves `host/path`, which is not absolute.
    if !rest.starts_with('/') {
        return Err(StoreUriError::FilePath(text.to_owned()));
    }
    Ok(path)
}

impl StoreUri {
    /// The S3 store this names, if it names one.
    #[must_use]
    pub fn as_s3(&self) -> Option<&S3Uri> {
        match self {
            Self::S3(s3) => Some(s3),
            Self::File(_) | Self::Registered => None,
        }
    }
}

impl S3Uri {
    /// The URI that names this store to another process (`--cold-store`): the bucket, the
    /// prefix, and the query keys [`S3Uri::parse`] takes, each percent-encoded, so that
    /// parsing it gives this store back. It spells out these fields and nothing else, so
    /// it can carry no key, token or other text a caller put in a field it does not name
    /// (a store URI has no place for a credential: any other query key is refused by
    /// the parser). `region` is always given; `conditional-put` and `min-put-rate` only
    /// when they differ from their defaults.
    #[must_use]
    pub fn node_uri(&self) -> String {
        let path = percent_encoding::utf8_percent_encode(&self.prefix, PATH).to_string();
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("endpoint", &self.endpoint);
        query.append_pair("region", &self.region);
        if self.conditional_put {
            query.append_pair("conditional-put", "true");
        }
        if self.min_put_rate != DEFAULT_MIN_PUT_RATE {
            query.append_pair("min-put-rate", &self.min_put_rate.to_string());
        }
        format!("s3://{}/{path}?{}", self.bucket, query.finish())
    }

    fn parse(text: &str, rest: &str) -> Result<Self, StoreUriError> {
        if rest.contains('#') {
            return Err(StoreUriError::S3Fragment(text.to_owned()));
        }
        let (location, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (bucket, path) = location.split_once('/').unwrap_or((location, ""));
        if bucket.is_empty() {
            return Err(StoreUriError::NoBucket(text.to_owned()));
        }
        let path = percent_decode_str(path).decode_utf8_lossy().into_owned();
        let prefix = match path.as_str() {
            "" => DEFAULT_S3_PREFIX.to_owned(),
            p if p.ends_with('/') => p.to_owned(),
            p => format!("{p}/"),
        };
        kbf_objstore::KeyPrefix::new(prefix.clone()).map_err(|source| StoreUriError::Prefix {
            prefix: prefix.clone(),
            source,
        })?;
        let mut uri = Self {
            bucket: bucket.to_owned(),
            prefix,
            endpoint: String::new(),
            region: DEFAULT_REGION.to_owned(),
            conditional_put: false,
            min_put_rate: DEFAULT_MIN_PUT_RATE,
        };
        let mut seen: Vec<&'static str> = Vec::new();
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            let key = match key.as_ref() {
                "endpoint" => "endpoint",
                "region" => "region",
                "conditional-put" => "conditional-put",
                "min-put-rate" => "min-put-rate",
                other => {
                    return Err(StoreUriError::UnknownQuery {
                        key: other.to_owned(),
                    });
                }
            };
            if seen.contains(&key) {
                return Err(StoreUriError::Repeated(key));
            }
            seen.push(key);
            uri.set(key, &value)?;
        }
        if uri.endpoint.is_empty() {
            return Err(StoreUriError::NoEndpoint(text.to_owned()));
        }
        Ok(uri)
    }

    fn set(&mut self, key: &'static str, value: &str) -> Result<(), StoreUriError> {
        let bad = |why| StoreUriError::BadValue {
            key,
            value: value.to_owned(),
            why,
        };
        match key {
            "endpoint" if value.is_empty() => return Err(bad("empty")),
            "endpoint" => self.endpoint = value.to_owned(),
            "region" if value.is_empty() => return Err(bad("empty")),
            "region" => self.region = value.to_owned(),
            "conditional-put" => {
                self.conditional_put = match value {
                    "true" => true,
                    "false" => false,
                    _ => return Err(bad("true or false")),
                };
            }
            _ => {
                self.min_put_rate = value
                    .parse()
                    .map_err(|_| bad("a positive number of bytes a second"))?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for StoreUri {
    /// The URI as the startup log names it (no secret is in one).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => write!(f, "file://{}", path.display()),
            Self::Registered => f.write_str(REGISTERED),
            Self::S3(s3) => write!(
                f,
                "s3://{}/{}?endpoint={}&region={}&conditional-put={}&min-put-rate={}",
                s3.bucket, s3.prefix, s3.endpoint, s3.region, s3.conditional_put, s3.min_put_rate
            ),
        }
    }
}

/// The hot store a start resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hot {
    /// The tier's directory.
    pub dir: PathBuf,
    /// The most bytes its block files may hold before they are flushed.
    pub budget: u64,
    /// The tier `--hot-store-abandon` names.
    pub abandon: Option<TierId>,
}

/// Where the flags put the server's bytes and metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stores {
    /// The durable object store.
    pub cold: StoreUri,
    /// The hot store's directory, budget and abandon request, if there is one.
    pub hot: Option<Hot>,
    /// The metadata log's directory.
    pub meta_dir: PathBuf,
    /// Whether `meta_dir` is under the hot store's directory (not `--meta-dir`).
    pub meta_in_hot: bool,
}

/// The metadata log's directory under a hot store directory.
pub const META_SUBDIR: &str = "meta";

/// Why the store flags do not make a server.
#[derive(Debug, thiserror::Error)]
pub enum StoresError {
    /// No `--cold-store`.
    #[error("--cold-store is required (file:///absolute/path or s3://bucket/prefix?endpoint=...)")]
    NoColdStore,
    /// A hot store that is not a directory.
    #[error(
        "--hot-store {0}: the hot store must be a file:// directory (the write tier is block \
         files on a local disk); an s3:// store is the cold store's"
    )]
    HotNotLocal(String),
    /// Nowhere to keep the metadata log.
    #[error(
        "--meta-dir is required: the metadata log lives in the hot store's directory when \
         --hot-store is a file:// store, and nowhere else is given"
    )]
    NoMetaDir,
    /// The hot store's filesystem cannot be read.
    #[error("--hot-store {dir}: {source}")]
    Space {
        /// The directory.
        dir: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The hot store's filesystem has nothing free.
    #[error("--hot-store {dir}: {available} bytes free of {total}: no room for a hot store")]
    HotNoRoom {
        /// The directory.
        dir: PathBuf,
        /// Free bytes.
        available: u64,
        /// The filesystem's size.
        total: u64,
    },
}

/// What the store flags say, before they are resolved.
#[derive(Clone, Copy, Debug)]
pub struct StoreFlags<'a> {
    /// `--cold-store`.
    pub cold: Option<&'a StoreUri>,
    /// `--hot-store`.
    pub hot: Option<&'a StoreUri>,
    /// `--hot-store-max-bytes`.
    pub hot_max_bytes: Option<u64>,
    /// `--hot-store-abandon`.
    pub hot_abandon: Option<u64>,
    /// `--meta-dir`.
    pub meta_dir: Option<&'a Path>,
}

impl StoreFlags<'_> {
    /// Resolves the flags: checks they make a server, finds the metadata directory,
    /// and sizes the hot store from `space`.
    ///
    /// # Errors
    /// [`StoresError`].
    pub fn resolve(&self, space: &dyn FreeSpace) -> Result<Stores, StoresError> {
        let cold = self.cold.ok_or(StoresError::NoColdStore)?.clone();
        let hot = match self.hot {
            None => None,
            Some(StoreUri::S3(_) | StoreUri::Registered) => {
                return Err(StoresError::HotNotLocal(
                    self.hot.map(ToString::to_string).unwrap_or_default(),
                ));
            }
            Some(StoreUri::File(dir)) => {
                let on = space.space(dir).map_err(|source| StoresError::Space {
                    dir: dir.clone(),
                    source,
                })?;
                // What the tier already holds is the tier's to reuse: counting only the
                // free space would shrink the budget at every restart by the bytes the
                // tier keeps. The metadata log in `<hot>/meta` is not the tier's.
                let held = |path: &Path| {
                    space.held(path).map_err(|source| StoresError::Space {
                        dir: path.to_owned(),
                        source,
                    })
                };
                let tier_bytes = held(dir)?.saturating_sub(held(&dir.join(META_SUBDIR))?);
                let budget = on
                    .with_held(tier_bytes)
                    .of_free(HOT_STORE_SHARE_PERCENT, self.hot_max_bytes);
                if budget == 0 {
                    return Err(StoresError::HotNoRoom {
                        dir: dir.clone(),
                        available: on.available,
                        total: on.total,
                    });
                }
                Some(Hot {
                    dir: dir.clone(),
                    budget,
                    abandon: self.hot_abandon.map(TierId::new),
                })
            }
        };
        let (meta_dir, meta_in_hot) = match (self.meta_dir, &hot) {
            (Some(dir), _) => (dir.to_owned(), false),
            (None, Some(hot)) => (hot.dir.join(META_SUBDIR), true),
            (None, None) => return Err(StoresError::NoMetaDir),
        };
        Ok(Stores {
            cold,
            hot,
            meta_dir,
            meta_in_hot,
        })
    }
}

impl Stores {
    /// The lines the startup log carries: every store, path and budget resolved.
    #[must_use]
    pub fn describe(&self) -> Vec<String> {
        let mut lines = vec![format!("cold store: {}", self.cold)];
        match &self.hot {
            Some(hot) => lines.push(format!(
                "hot store: file://{} (budget {} bytes)",
                hot.dir.display(),
                hot.budget
            )),
            None => lines.push("hot store: none (uploads go straight to the cold store)".into()),
        }
        let whose = if self.meta_in_hot {
            "in the hot store"
        } else {
            "--meta-dir"
        };
        lines.push(format!(
            "metadata log: {} ({whose})",
            self.meta_dir.display()
        ));
        lines
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::Path;

    use kbf_threads::disk::Space;

    use super::*;

    const GIB: u64 = 1 << 30;

    fn uri(text: &str) -> Result<StoreUri, StoreUriError> {
        text.parse()
    }

    fn s3(text: &str) -> S3Uri {
        uri(text)
            .expect("a store")
            .as_s3()
            .expect("an s3 store")
            .clone()
    }

    /// Catches a URI for a node that does not read back as the store it was made from:
    /// a prefix or endpoint with characters a URI treats specially, a default setting
    /// written out (so a node and the frontend would print different URIs for one store),
    /// or a non-default one left out.
    #[test]
    fn a_node_uri_reads_back_as_the_store_it_names() {
        let stores = [
            s3("s3://b/?endpoint=http://h:9000"),
            s3(
                "s3://b/a/b/?endpoint=http://h:9000&region=eu-1&conditional-put=true&min-put-rate=7",
            ),
            S3Uri {
                prefix: "x-y_z.~1/a/".to_owned(),
                ..s3("s3://b/?endpoint=http://h:9000")
            },
        ];
        for store in stores {
            let text = store.node_uri();
            assert_eq!(uri(&text), Ok(StoreUri::S3(store.clone())), "{text}");
        }
        let plain = s3("s3://b/?endpoint=http://h:9000").node_uri();
        assert_eq!(
            plain,
            "s3://b/kbf/?endpoint=http%3A%2F%2Fh%3A9000&region=us-east-1"
        );
    }

    /// Catches a `file://` URI read as anything but its absolute path: the host form
    /// (`file://host/x`, a relative path in disguise), a relative path, a query or a
    /// fragment (they would silently become part of a directory name), and an escaped
    /// path not decoded.
    #[test]
    fn a_file_uri_is_an_absolute_path() {
        assert_eq!(
            uri("file:///var/lib/kbf"),
            Ok(StoreUri::File("/var/lib/kbf".into()))
        );
        assert_eq!(uri("FILE:///a"), Ok(StoreUri::File("/a".into())));
        assert_eq!(uri("file:///a%20b"), Ok(StoreUri::File("/a b".into())));
        assert_eq!(uri("file:///a%3Fb"), Ok(StoreUri::File("/a?b".into())));
        assert_eq!(uri("file:///a").expect("a store").as_s3(), None);
        for bad in [
            "file://host/x",
            "file://x",
            "file://",
            "file://relative/path",
        ] {
            assert!(matches!(uri(bad), Err(StoreUriError::FilePath(_))), "{bad}");
        }
        for bad in ["file:///a?x=1", "file:///a#frag"] {
            assert!(
                matches!(uri(bad), Err(StoreUriError::FileQuery(_))),
                "{bad}"
            );
        }
    }

    /// Catches an S3 URI whose parts do not reach the store: the bucket, the prefix
    /// (given a trailing `/`, the default `kbf/` for none), the endpoint, region,
    /// conditional puts and put rate from the query, and their defaults.
    #[test]
    fn an_s3_uri_carries_the_bucket_prefix_and_query() {
        let plain = s3("s3://bucket?endpoint=http://127.0.0.1:9000");
        assert_eq!(
            plain,
            S3Uri {
                bucket: "bucket".into(),
                prefix: "kbf/".into(),
                endpoint: "http://127.0.0.1:9000".into(),
                region: "us-east-1".into(),
                conditional_put: false,
                min_put_rate: NonZeroU64::new(1 << 20).expect("1 MiB"),
            }
        );
        let full = s3(
            "s3://b/farm/cas?endpoint=http://e:9&region=eu-west-1&conditional-put=true&min-put-rate=4096",
        );
        assert_eq!(full.bucket, "b");
        assert_eq!(full.prefix, "farm/cas/");
        assert_eq!(full.endpoint, "http://e:9");
        assert_eq!(full.region, "eu-west-1");
        assert!(full.conditional_put);
        assert_eq!(full.min_put_rate.get(), 4096);
        assert_eq!(s3("s3://b/p/?endpoint=http://e").prefix, "p/");
        assert_eq!(s3("s3://b/?endpoint=http://e").prefix, "kbf/");
        let off = s3("s3://b?endpoint=http://e&conditional-put=false");
        assert!(!off.conditional_put, "an explicit false");
    }

    /// Catches what an S3 URI must refuse: no bucket, no endpoint (the one setting
    /// with no default), an unknown query key (a typo that would silently do nothing),
    /// a repeated key, a rate of 0 or not a number, a boolean that is neither, an empty
    /// endpoint, a prefix the store refuses (spaces), and a fragment.
    #[test]
    fn a_bad_s3_uri_is_refused_naming_the_problem() {
        let refused = |text: &str| uri(text).expect_err(text);
        assert!(matches!(
            refused("s3://?endpoint=http://e"),
            StoreUriError::NoBucket(_)
        ));
        assert!(matches!(
            refused("s3:///p?endpoint=http://e"),
            StoreUriError::NoBucket(_)
        ));
        assert!(matches!(refused("s3://b"), StoreUriError::NoEndpoint(_)));
        assert!(matches!(
            refused("s3://b/p?region=r"),
            StoreUriError::NoEndpoint(_)
        ));
        assert!(matches!(
            refused("s3://b?endpoint=http://e&conditional=true"),
            StoreUriError::UnknownQuery { .. }
        ));
        assert!(matches!(
            refused("s3://b?endpoint=http://e&endpoint=http://f"),
            StoreUriError::Repeated("endpoint")
        ));
        for query in [
            "min-put-rate=0",
            "min-put-rate=fast",
            "conditional-put=yes",
            "region=",
        ] {
            let text = format!("s3://b?endpoint=http://e&{query}");
            let error = refused(&text);
            assert!(
                matches!(error, StoreUriError::BadValue { .. }),
                "{text}: {error}"
            );
        }
        assert!(matches!(
            refused("s3://b?endpoint="),
            StoreUriError::BadValue {
                key: "endpoint",
                ..
            }
        ));
        assert!(matches!(
            refused("s3://b/no%20spaces?endpoint=http://e"),
            StoreUriError::Prefix { .. }
        ));
        assert!(matches!(
            refused("s3://b?endpoint=http://e#x"),
            StoreUriError::S3Fragment(_)
        ));
    }

    /// Catches schemes accepted that the server does not take: `memory:` (a store in
    /// memory is a file:// path on a tmpfs, and the refusal says so), an unknown scheme
    /// (`gs://`, `http://`), and a bare path with no scheme at all.
    #[test]
    fn memory_and_unknown_schemes_are_refused() {
        let memory = uri("memory://").expect_err("memory");
        assert_eq!(memory, StoreUriError::Memory);
        assert!(memory.to_string().contains("tmpfs"), "{memory}");
        assert_eq!(uri("memory:///x"), Err(StoreUriError::Memory));
        assert_eq!(
            uri("gs://b/p"),
            Err(StoreUriError::UnknownScheme("gs".into()))
        );
        assert!(matches!(
            uri("http://e"),
            Err(StoreUriError::UnknownScheme(_))
        ));
        assert!(matches!(
            uri("/var/lib/kbf"),
            Err(StoreUriError::NoScheme(_))
        ));
        assert!(matches!(uri(""), Err(StoreUriError::NoScheme(_))));
    }

    struct Fake(Space);

    impl FreeSpace for Fake {
        fn space(&self, _: &Path) -> io::Result<Space> {
            Ok(self.0)
        }
    }

    const DISK: Space = Space {
        total: 1000 * GIB,
        available: 500 * GIB,
    };

    fn flags<'a>(
        cold: Option<&'a StoreUri>,
        hot: Option<&'a StoreUri>,
        meta: Option<&'a Path>,
    ) -> StoreFlags<'a> {
        StoreFlags {
            cold,
            hot,
            hot_max_bytes: None,
            hot_abandon: None,
            meta_dir: meta,
        }
    }

    fn file(path: &str) -> StoreUri {
        StoreUri::File(path.into())
    }

    /// Catches the metadata log not living inside the hot directory (the operator would
    /// have to name a second directory for the one disk), the hot store budget not the
    /// free-space share (80% of the free space less the 5% floor), an abandon request
    /// lost, and a hot store with a `meta` subdirectory wrongly placed.
    #[test]
    fn a_file_hot_store_holds_the_metadata_log_and_is_sized_from_free_space() {
        let (cold, hot) = (file("/cold"), file("/hot"));
        let stores = flags(Some(&cold), Some(&hot), None)
            .resolve(&Fake(DISK))
            .expect("stores");
        let tier = stores.hot.clone().expect("a hot store");
        assert_eq!(tier.dir, Path::new("/hot"));
        assert_eq!(tier.budget, (500 - 50) * GIB * 8 / 10);
        assert_eq!(tier.abandon, None);
        assert_eq!(stores.meta_dir, Path::new("/hot/meta"));
        assert!(stores.meta_in_hot);
        assert_eq!(stores.cold, cold);
        let with = StoreFlags {
            hot_max_bytes: Some(7 * GIB),
            hot_abandon: Some(3),
            ..flags(Some(&cold), Some(&hot), None)
        };
        let tier = with.resolve(&Fake(DISK)).expect("stores").hot.expect("hot");
        assert_eq!(tier.budget, 7 * GIB);
        assert_eq!(tier.abandon, Some(TierId::new(3)));
    }

    /// Catches a server that starts with no home for the metadata log (it would write
    /// it somewhere unplanned or in memory): without a hot store, or with an s3 cold
    /// store only, `--meta-dir` is required; given, it wins over the hot store's
    /// directory (the log on its own disk, as the deploy docs advise).
    #[test]
    fn the_metadata_directory_is_required_without_a_file_hot_store() {
        let (cold, hot) = (file("/cold"), file("/hot"));
        let error = flags(Some(&cold), None, None)
            .resolve(&Fake(DISK))
            .expect_err("no meta dir");
        assert!(matches!(error, StoresError::NoMetaDir), "{error}");
        assert!(error.to_string().contains("--meta-dir"), "{error}");
        let meta = Path::new("/meta");
        let stores = flags(Some(&cold), None, Some(meta))
            .resolve(&Fake(DISK))
            .expect("stores");
        assert_eq!((stores.hot, stores.meta_dir.as_path()), (None, meta));
        assert!(!stores.meta_in_hot);
        let stores = flags(Some(&cold), Some(&hot), Some(meta))
            .resolve(&Fake(DISK))
            .expect("stores");
        assert_eq!(stores.meta_dir, meta);
        assert!(!stores.meta_in_hot);
        assert!(stores.hot.is_some());
    }

    /// Catches the other refusals: no cold store, an s3 hot store (the write tier is
    /// block files on a local disk), and a hot store filesystem with nothing free (a
    /// budget of 0 would make a tier that takes nothing).
    #[test]
    fn a_store_set_that_cannot_run_is_refused() {
        let (cold, hot) = (file("/cold"), file("/hot"));
        let none = flags(None, Some(&hot), None).resolve(&Fake(DISK));
        assert!(matches!(none, Err(StoresError::NoColdStore)));
        let remote = StoreUri::S3(s3("s3://b?endpoint=http://e"));
        let error = flags(Some(&cold), Some(&remote), Some(Path::new("/m")))
            .resolve(&Fake(DISK))
            .expect_err("s3 hot");
        assert!(matches!(error, StoresError::HotNotLocal(_)), "{error}");
        assert!(error.to_string().contains("s3://b"), "{error}");
        let full = Space {
            total: 1000 * GIB,
            available: 0,
        };
        let error = flags(Some(&cold), Some(&hot), None)
            .resolve(&Fake(full))
            .expect_err("no room");
        assert!(matches!(error, StoresError::HotNoRoom { .. }), "{error}");
    }

    /// Catches a startup log that leaves out a store, a path or a budget.
    #[test]
    fn the_start_log_lists_every_store_path_and_budget() {
        let (cold, hot) = (file("/cold"), file("/hot"));
        let stores = flags(Some(&cold), Some(&hot), None)
            .resolve(&Fake(DISK))
            .expect("stores");
        let lines = stores.describe();
        assert_eq!(lines[0], "cold store: file:///cold");
        assert!(
            lines[1].starts_with("hot store: file:///hot (budget 3"),
            "{lines:?}"
        );
        assert_eq!(lines[2], "metadata log: /hot/meta (in the hot store)");
        let meta = Path::new("/m");
        let remote = StoreUri::S3(s3("s3://b/p?endpoint=http://e:9"));
        let stores = flags(Some(&remote), None, Some(meta))
            .resolve(&Fake(DISK))
            .expect("stores");
        let lines = stores.describe();
        assert!(
            lines[0].starts_with("cold store: s3://b/p/?endpoint=http://e:9"),
            "{lines:?}"
        );
        assert!(lines[1].starts_with("hot store: none"), "{lines:?}");
        assert_eq!(lines[2], "metadata log: /m (--meta-dir)");
    }

    /// A disk with a hot directory on it: the tier's bytes (and the log's, under
    /// `meta`) are used space, so `available` is what is left beside them.
    struct Used {
        total: u64,
        others: u64,
        tier: u64,
        meta: u64,
    }

    impl FreeSpace for Used {
        fn space(&self, _: &Path) -> io::Result<Space> {
            Ok(Space {
                total: self.total,
                available: self.total - self.others - self.tier - self.meta,
            })
        }

        fn held(&self, path: &Path) -> io::Result<u64> {
            Ok(if path.ends_with(META_SUBDIR) {
                self.meta
            } else {
                self.tier + self.meta
            })
        }
    }

    /// Catches a hot budget that shrinks at a restart by what the tier holds (a share
    /// of free space alone: a full tier would restart with almost no budget), and one
    /// that counts the metadata log beside the tier as the tier's bytes. A fresh start
    /// and a restart with the tier holding 200 GiB (and a 2 GiB log) give one budget,
    /// and `--hot-store-max-bytes` still caps it.
    #[test]
    fn a_restart_gives_the_hot_store_the_budget_of_a_fresh_start() {
        let (cold, hot) = (file("/cold"), file("/hot"));
        let budget = |tier: u64, max: Option<u64>| {
            let disk = Used {
                total: 1000 * GIB,
                others: 300 * GIB,
                tier,
                meta: 0,
            };
            let flags = StoreFlags {
                hot_max_bytes: max,
                ..flags(Some(&cold), Some(&hot), Some(Path::new("/meta")))
            };
            flags
                .resolve(&disk)
                .expect("stores")
                .hot
                .expect("hot")
                .budget
        };
        let fresh = budget(0, None);
        assert_eq!(fresh, (700 - 50) * GIB * 8 / 10);
        assert_eq!(
            budget(200 * GIB, None),
            fresh,
            "a restart shrank the budget"
        );
        assert_eq!(budget(200 * GIB, Some(7 * GIB)), 7 * GIB);
        // The log under `<hot>/meta` is used space that is not the tier's.
        let beside = Used {
            total: 1000 * GIB,
            others: 300 * GIB,
            tier: 0,
            meta: 2 * GIB,
        };
        let with_log = flags(Some(&cold), Some(&hot), None)
            .resolve(&beside)
            .expect("stores")
            .hot
            .expect("hot")
            .budget;
        assert_eq!(with_log, (698 - 50) * GIB * 8 / 10);
    }

    /// A disk whose size reads but whose hot directory does not.
    struct Unwalkable;

    impl FreeSpace for Unwalkable {
        fn space(&self, _: &Path) -> io::Result<Space> {
            Ok(DISK)
        }

        fn held(&self, _: &Path) -> io::Result<u64> {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }
    }

    /// Catches a hot directory that cannot be measured sized anyway (a guess at what
    /// the tier holds could overfill the disk): the start is refused naming it.
    #[test]
    fn an_unreadable_hot_directory_is_refused() {
        let (cold, hot) = (file("/cold"), file("/hot"));
        let error = flags(Some(&cold), Some(&hot), None)
            .resolve(&Unwalkable)
            .expect_err("unreadable");
        assert!(matches!(error, StoresError::Space { .. }), "{error}");
        assert!(error.to_string().contains("/hot"), "{error}");
    }

    struct Broken;

    impl FreeSpace for Broken {
        fn space(&self, _: &Path) -> io::Result<Space> {
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        }
    }

    /// Catches a hot store whose filesystem cannot be read sized as if it could: the
    /// start is refused naming the directory (a guessed budget could fill the disk).
    #[test]
    fn an_unreadable_hot_store_filesystem_is_refused_naming_the_directory() {
        let (cold, hot) = (file("/cold"), file("/hot"));
        let error = flags(Some(&cold), Some(&hot), None)
            .resolve(&Broken)
            .expect_err("unreadable");
        assert!(matches!(error, StoresError::Space { .. }), "{error}");
        assert!(error.to_string().contains("/hot"), "{error}");
        // Without a hot store nothing is measured.
        let meta = Path::new("/m");
        flags(Some(&cold), None, Some(meta))
            .resolve(&Broken)
            .expect("no hot store, no measuring");
    }
}
