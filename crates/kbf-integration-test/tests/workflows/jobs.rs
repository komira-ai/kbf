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

/// The job `job`'s `name:` (what its check is called); `None` without one. An error when it is
/// not a string.
pub fn job_name(text: &str, job: &str) -> Result<Option<String>, String> {
    let root = yaml::load(text)?;
    match get(job_of(&root, job)?, "name") {
        None => Ok(None),
        Some(n) => n
            .as_str()
            .map(|n| Some(n.to_owned()))
            .ok_or_else(|| format!("the `name` of job `{job}` is not a string")),
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

/// Whether the job `job` has the key `key` at all (`secrets`, for a job that calls a workflow).
pub fn job_has_key(text: &str, job: &str, key: &str) -> Result<bool, String> {
    let root = yaml::load(text)?;
    Ok(get(job_of(&root, job)?, key).is_some())
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
    trigger_filters(text, "push")
}

/// The filters under the workflow's `trigger` mapping as `(key, values)`: every key
/// (`types`, `branches`, `workflows`, ...) with its list of strings. An error when `on:`
/// has no such mapping.
pub fn trigger_filters(text: &str, trigger: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let root = yaml::load(text)?;
    let on = get(top_of(&root)?, "on").ok_or("the workflow has no `on:`")?;
    let Value::Map(on) = &on.value else {
        return Err("`on:` is not a mapping".to_owned());
    };
    let Some(Value::Map(filters)) = get(on, trigger).map(|p| &p.value) else {
        return Err(format!("`on:` has no `{trigger}:` mapping"));
    };
    filters
        .iter()
        .map(|(k, v)| {
            let key = k
                .as_str()
                .ok_or_else(|| format!("a `{trigger}` key is not a string"))?
                .to_owned();
            let values = match &v.value {
                Value::Seq(items) => items
                    .iter()
                    .map(|n| n.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>(),
                _ => v.as_str().map(|s| vec![s.to_owned()]),
            };
            let values = values.ok_or_else(|| format!("`{trigger}.{key}` is not strings"))?;
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

/// The entries of the matrix of the job `job`, each as its `(name, value)` pairs in file order,
/// the matrix being a list under `strategy.matrix.include` and nothing else (a matrix with axes
/// of its own expands to a product this reader does not compute). An error for any other shape,
/// an entry that is not a mapping of scalars, or no entry at all.
pub fn job_matrix(text: &str, job: &str) -> Result<Vec<Vec<(String, String)>>, String> {
    let root = yaml::load(text)?;
    let found = job_of(&root, job)?;
    let Some(Value::Map(strategy)) = get(found, "strategy").map(|s| &s.value) else {
        return Err(format!("the job `{job}` has no `strategy` mapping"));
    };
    let Some(Value::Map(matrix)) = get(strategy, "matrix").map(|m| &m.value) else {
        return Err(format!(
            "the `strategy` of job `{job}` has no `matrix` mapping"
        ));
    };
    if matrix.iter().any(|(k, _)| k.as_str() != Some("include")) {
        return Err(format!(
            "the matrix of job `{job}` has a key besides `include`, which this reader does not expand"
        ));
    }
    let Some(Value::Seq(entries)) = get(matrix, "include").map(|i| &i.value) else {
        return Err(format!("the matrix of job `{job}` has no `include` list"));
    };
    if entries.is_empty() {
        return Err(format!("the matrix of job `{job}` has no entry"));
    }
    entries
        .iter()
        .map(|entry| match &entry.value {
            Value::Map(pairs) => pairs
                .iter()
                .map(|(k, v)| match (k.as_str(), v.as_str()) {
                    (Some(k), Some(v)) => Ok((k.to_owned(), v.to_owned())),
                    _ => Err(format!(
                        "a matrix entry of job `{job}` has a value that is not a scalar"
                    )),
                })
                .collect(),
            _ => Err(format!("a matrix entry of job `{job}` is not a mapping")),
        })
        .collect()
}

/// The job `job`'s `runs-on:` when it is a string; `None` without one or for any other shape (a
/// list or a mapping), which no seed build job may use.
pub fn job_runs_on(text: &str, job: &str) -> Result<Option<String>, String> {
    let root = yaml::load(text)?;
    Ok(get(job_of(&root, job)?, "runs-on")
        .and_then(Node::as_str)
        .map(str::to_owned))
}

/// The reusable workflow the job `job` calls (`uses:`), `None` for a job with steps. An error
/// when it is not a string.
pub fn job_uses(text: &str, job: &str) -> Result<Option<String>, String> {
    let root = yaml::load(text)?;
    let found = job_of(&root, job)?;
    match get(found, "uses") {
        None => Ok(None),
        Some(u) => u
            .as_str()
            .map(|u| Some(u.to_owned()))
            .ok_or_else(|| format!("the `uses` of job `{job}` is not a string")),
    }
}

/// The inputs the job `job` passes to the reusable workflow it calls (`with:`) as `(name,
/// value)`; empty without one. An error when it is not a mapping of strings.
pub fn job_with(text: &str, job: &str) -> Result<Vec<(String, String)>, String> {
    let root = yaml::load(text)?;
    let found = job_of(&root, job)?;
    match get(found, "with").map(|w| &w.value) {
        None => Ok(Vec::new()),
        Some(Value::Map(with)) => with
            .iter()
            .map(|(k, v)| match (k.as_str(), v.as_str()) {
                (Some(k), Some(v)) => Ok((k.to_owned(), v.to_owned())),
                _ => Err(format!(
                    "a `with` entry of job `{job}` is not a string pair"
                )),
            })
            .collect(),
        Some(_) => Err(format!("the `with` of job `{job}` is not a mapping")),
    }
}

/// The `defaults.run.shell` of the mapping `m` (a workflow's top level or a job): `None` without
/// one. An error when `defaults` or `run` is not a mapping or `shell` not a string.
fn defaults_shell(m: &[(Node, Node)], what: &str) -> Result<Option<String>, String> {
    let Some(defaults) = get(m, "defaults") else {
        return Ok(None);
    };
    let Value::Map(defaults) = &defaults.value else {
        return Err(format!("the `defaults` of {what} is not a mapping"));
    };
    let Some(run) = get(defaults, "run") else {
        return Ok(None);
    };
    let Value::Map(run) = &run.value else {
        return Err(format!("the `defaults.run` of {what} is not a mapping"));
    };
    match get(run, "shell") {
        None => Ok(None),
        Some(sh) => sh
            .as_str()
            .map(|v| Some(v.to_owned()))
            .ok_or_else(|| format!("the `defaults.run.shell` of {what} is not a string")),
    }
}

/// The workflow's `defaults.run.shell`; `None` without one.
pub fn top_defaults_shell(text: &str) -> Result<Option<String>, String> {
    let root = yaml::load(text)?;
    defaults_shell(top_of(&root)?, "the workflow")
}

/// The workflow's own top-level `concurrency:` as `(group, cancel-in-progress)`, both as
/// written (`(absent)` for a missing cancel); `None` without one. An error when it is not a mapping of strings with a `group`.
pub fn concurrency(text: &str) -> Result<Option<(String, String)>, String> {
    let root = yaml::load(text)?;
    let Some(c) = get(top_of(&root)?, "concurrency") else {
        return Ok(None);
    };
    let Value::Map(c) = &c.value else {
        return Err("the top-level `concurrency` is not a mapping".to_owned());
    };
    let group = get(c, "group")
        .and_then(Node::as_str)
        .ok_or("the top-level `concurrency` has no string `group`")?;
    let cancel = get(c, "cancel-in-progress")
        .and_then(Node::as_str)
        .unwrap_or("(absent)");
    Ok(Some((group.to_owned(), cancel.to_owned())))
}

/// The workflow's own top-level `env:` as `(name, value)`; empty without one. An error when
/// it is not a mapping of strings.
pub fn top_env(text: &str) -> Result<Vec<(String, String)>, String> {
    let root = yaml::load(text)?;
    match get(top_of(&root)?, "env").map(|e| &e.value) {
        None => Ok(Vec::new()),
        Some(Value::Map(env)) => env
            .iter()
            .map(|(k, v)| match (k.as_str(), v.as_str()) {
                (Some(k), Some(v)) => Ok((k.to_owned(), v.to_owned())),
                _ => Err("a top-level `env` entry is not a string pair".to_owned()),
            })
            .collect(),
        Some(_) => Err("the top-level `env` is not a mapping".to_owned()),
    }
}
