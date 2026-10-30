//! The errors of the tool. A failed request to a store is a measurement, not an error here
//! (see [`crate::s3::Outcome`]); an error is something that stops a run.

use std::path::PathBuf;

use crate::config::ConfigError;

/// Why a verb or a cell stopped.
#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    /// The verb or module is a stub of the scaffold; the text names it.
    #[error("{0}: not yet implemented")]
    NotYet(&'static str),
    /// The flags and the run file do not make a configuration.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A store's address or name is not one the S3 client accepts.
    #[error("store configuration: {0}")]
    Store(#[from] kbf_objstore::s3::S3ConfigError),
    /// An object key is not one the S3 client accepts.
    #[error("object key: {0}")]
    Key(#[from] kbf_objstore::KeyError),
    /// A range or a page size of zero or beyond what the S3 client allows.
    #[error("{0}")]
    Request(&'static str),
    /// A file of the run could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// What the operating system said.
        #[source]
        source: std::io::Error,
    },
    /// The request log could not be written. The run stops: a harness-side log with a gap
    /// proves nothing.
    #[error("request log: {0}")]
    Log(#[source] std::io::Error),
    /// A cell stopped the run on purpose (the load gate, a stop file, a byte budget that left
    /// no room for the next level). Its result files are written first; the text says why.
    #[error("halted: {0}")]
    Halted(String),
    /// A result file is not JSON of the schema this build writes.
    #[error("result file: {0}")]
    Result(String),
}
