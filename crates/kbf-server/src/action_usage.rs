//! What kind of action an Execute runs, read from its argv and output paths: its build target
//! and whether it builds or tests ([`Shape`]). The executor labels a job's size class with it
//! ([`crate::executor::SizeClass`]).

mod shape;

pub use shape::{Shape, ShapeKey, Target, TargetKind};
