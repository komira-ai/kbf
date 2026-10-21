//! Readers of the job-level and trigger keys the release workflow lint
//! (`release_workflow`) checks: a job's `environment`, `needs`, `permissions` and
//! `outputs`, and the filters under `push`. Keys are matched ignoring ASCII case, as
//! the workflow lint does; a shape that is not the expected one is an error, never an
//! empty answer, so a rewritten key cannot make a check pass by reading nothing.

use super::yaml::{self, Node, Value};
use super::{get, top_of};

/// The mapping of the job `job`.
fn job_of<'a>(root: &'a Node, job: &str) -> Result<&'a [(Node, Node)], String> {
    let Some(Value::Map(jobs)) = get(top_of(root)?, "jobs").map(|j| &j.value) else {
        return Err("the workflow has no `jobs` mapping".to_owned());
    };
    match get(jobs, job).map(|j| &j.value) {
        Some(Value::Map(found)) => Ok(found),
        _ => Err(format!("the workflow has no job `{job}`")),
    }
}

/// The environment the job `job` runs in: `None` without `environment:`; the name when
/// it is a string or a mapping with a string `name:`. An error for any other shape.
pub fn job_environment(text: &str, job: &str) -> Result<Option<String>, String> {
    let root = yaml::load(text)?;
    let found = job_of(&root, job)?;
    let Some(env) = get(found, "environment") else {
        return Ok(None);
    };
    let name = match &env.value {
        Value::Map(m) => get(m, "name").and_then(Node::as_str),
        _ => env.as_str(),
    };
    name.map(|n| Some(n.to_owned()))
        .ok_or_else(|| format!("the `environment` of job `{job}` has no string name"))
}

/// The jobs `job` needs, in order (`needs:` is a string or a list of strings).
pub fn job_needs(text: &str, job: &str) -> Result<Vec<String>, String> {
    let root = yaml::load(text)?;
    let found = job_of(&root, job)?;
    let Some(needs) = get(found, "needs") else {
        return Ok(Vec::new());
    };
    let items: Vec<&Node> = match &needs.value {
        Value::Seq(items) => items.iter().collect(),
        _ => vec![needs],
    };
    items
        .iter()
        .map(|n| {
            n.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("a `needs` entry of job `{job}` is not a string"))
        })
        .collect()
}

/// The scopes the job `job` sets under its own `permissions:`, as `(scope, level)`; an
/// empty list without one. An error when it is not a mapping of strings.
pub fn job_permission_levels(text: &str, job: &str) -> Result<Vec<(String, String)>, String> {
    let root = yaml::load(text)?;
    let found = job_of(&root, job)?;
    let Some(p) = get(found, "permissions") else {
        return Ok(Vec::new());
    };
    let Value::Map(scopes) = &p.value else {
        return Err(format!("the `permissions` of job `{job}` is not a mapping"));
    };
    scopes
        .iter()
        .map(|(k, v)| match (k.as_str(), v.as_str()) {
            (Some(k), Some(v)) => Ok((k.to_owned(), v.to_owned())),
            _ => Err(format!("a permission of job `{job}` is not a string pair")),
        })
        .collect()
}

/// The names of the keys under the job `job`'s `outputs:`; empty without any.
pub fn job_outputs(text: &str, job: &str) -> Result<Vec<(String, String)>, String> {
    let root = yaml::load(text)?;
    let found = job_of(&root, job)?;
    match get(found, "outputs").map(|o| &o.value) {
        None => Ok(Vec::new()),
        Some(Value::Map(outputs)) => Ok(outputs
            .iter()
            .filter_map(|(k, v)| Some((k.as_str()?.to_owned(), v.as_str()?.to_owned())))
            .collect()),
        Some(_) => Err(format!("the `outputs` of job `{job}` is not a mapping")),
    }
}

/// The filters under the workflow's `push` trigger as `(key, values)`: every key
/// (`tags`, `branches`, `paths`, ...) with its list of strings. An error when `on:` has
/// no `push` mapping.
pub fn push_filters(text: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let root = yaml::load(text)?;
    let on = get(top_of(&root)?, "on").ok_or("the workflow has no `on:`")?;
    let Value::Map(on) = &on.value else {
        return Err("`on:` is not a mapping".to_owned());
    };
    let Some(Value::Map(push)) = get(on, "push").map(|p| &p.value) else {
        return Err("`on:` has no `push:` mapping".to_owned());
    };
    push.iter()
        .map(|(k, v)| {
            let key = k.as_str().ok_or("a `push` key is not a string")?.to_owned();
            let values = match &v.value {
                Value::Seq(items) => items
                    .iter()
                    .map(|n| n.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>(),
                _ => v.as_str().map(|s| vec![s.to_owned()]),
            };
            let values = values.ok_or_else(|| format!("`push.{key}` is not strings"))?;
            Ok((key, values))
        })
        .collect()
}

/// The job `job`'s own `env:` as `(name, value)`; empty without one. An error when it is
/// not a mapping of strings.
pub fn job_env(text: &str, job: &str) -> Result<Vec<(String, String)>, String> {
    let root = yaml::load(text)?;
    let found = job_of(&root, job)?;
    match get(found, "env").map(|e| &e.value) {
        None => Ok(Vec::new()),
        Some(Value::Map(env)) => env
            .iter()
            .map(|(k, v)| match (k.as_str(), v.as_str()) {
                (Some(k), Some(v)) => Ok((k.to_owned(), v.to_owned())),
                _ => Err(format!(
                    "an `env` entry of job `{job}` is not a string pair"
                )),
            })
            .collect(),
        Some(_) => Err(format!("the `env` of job `{job}` is not a mapping")),
    }
}

/// The inputs of the `workflow_dispatch` trigger as `(name, required)`. An error when
/// `on:` has no `workflow_dispatch` mapping with an `inputs:` mapping of mappings.
pub fn dispatch_inputs(text: &str) -> Result<Vec<(String, bool)>, String> {
    let root = yaml::load(text)?;
    let on = get(top_of(&root)?, "on").ok_or("the workflow has no `on:`")?;
    let Value::Map(on) = &on.value else {
        return Err("`on:` is not a mapping".to_owned());
    };
    let Some(Value::Map(dispatch)) = get(on, "workflow_dispatch").map(|d| &d.value) else {
        return Err("`on:` has no `workflow_dispatch:` mapping".to_owned());
    };
    let Some(Value::Map(inputs)) = get(dispatch, "inputs").map(|i| &i.value) else {
        return Err("`workflow_dispatch:` has no `inputs:` mapping".to_owned());
    };
    inputs
        .iter()
        .map(|(k, v)| {
            let name = k.as_str().ok_or("a dispatch input name is not a string")?;
            let Value::Map(spec) = &v.value else {
                return Err(format!("the dispatch input `{name}` is not a mapping"));
            };
            let required = get(spec, "required").and_then(Node::as_str) == Some("true");
            Ok((name.to_owned(), required))
        })
        .collect()
}
