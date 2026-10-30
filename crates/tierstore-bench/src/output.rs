//! The files of a run's results: one JSON per arm (`<cell>-<arm>.json`) and the CSV of raw
//! samples (`samples.csv`), both in the result directory. The shapes are in [`crate::result`].

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::error::BenchError;
use crate::result::{CellResult, Provenance, Sample};

/// The name of the CSV of samples.
pub const SAMPLES_FILE: &str = "samples.csv";

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> BenchError + '_ {
    move |source| BenchError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Writes `result` to `<dir>/<cell>-<arm>.json`, replacing what is there.
///
/// # Errors
/// [`BenchError::Io`] if the file cannot be written.
pub fn write_result(dir: &Path, result: &CellResult) -> Result<PathBuf, BenchError> {
    let path = dir.join(format!("{}-{}.json", result.cell.as_str(), result.arm));
    std::fs::write(&path, result.to_json()).map_err(io(&path))?;
    Ok(path)
}

/// Appends samples to `<dir>/samples.csv`, writing the header when the file is new.
pub struct SampleWriter {
    path: PathBuf,
    file: File,
}

impl SampleWriter {
    /// Opens the file for appending.
    ///
    /// # Errors
    /// [`BenchError::Io`] if it cannot be opened or written.
    pub fn open(dir: &Path) -> Result<Self, BenchError> {
        let path = dir.join(SAMPLES_FILE);
        let mut file = File::options()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io(&path))?;
        if file.metadata().map_err(io(&path))?.len() == 0 {
            writeln!(file, "{}", Sample::CSV_HEADER).map_err(io(&path))?;
        }
        Ok(Self { path, file })
    }

    /// Appends one row per sample and flushes.
    ///
    /// # Errors
    /// [`BenchError::Io`] if the write fails.
    pub fn append(
        &mut self,
        provenance: &Provenance,
        samples: &[Sample],
    ) -> Result<(), BenchError> {
        let mut text = String::new();
        for sample in samples {
            text.push_str(&sample.csv_row(provenance));
            text.push('\n');
        }
        self.file
            .write_all(text.as_bytes())
            .and_then(|()| self.file.flush())
            .map_err(io(&self.path))
    }
}
