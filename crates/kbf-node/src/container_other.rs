use super::{Cli, Error, Node};
use crate::data_dir::Plan;

pub(super) fn add<C>(_cli: &Cli, _node: &mut Node<C>, _plan: &Option<Plan>) -> Result<(), Error> {
    Err("the container driver runs on Linux only; use --driver native".into())
}
