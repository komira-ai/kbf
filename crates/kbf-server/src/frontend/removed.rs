//! The flags `kbf frontend` removed: the three sets that stay as hidden flags refusing any value
//! ([`executor_flags`], [`task_flags`], the job read listener's store grant flags), with what each
//! set's refusal tells, in one list. `kbf-cli`'s settings file reads it to name every removed key
//! of a file at once, and the version line lists the names.

use super::{executor_flags, job_read::store_grant, task_flags};

/// Every removed flag of the frontend with the reason its refusal gives, in the order of the three
/// sets. The name is the long name, with no dashes before it.
pub fn flags() -> impl Iterator<Item = (&'static str, &'static str)> {
    let set = |names: &'static [&'static str], why: &'static str| {
        names.iter().map(move |name| (*name, why))
    };
    set(&executor_flags::REMOVED, executor_flags::REMOVED_WHY)
        .chain(set(&task_flags::REMOVED, task_flags::REMOVED_WHY))
        .chain(set(&store_grant::REMOVED, store_grant::REMOVED_WHY))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::flags;

    /// The long names of the flags of the frontend's command whose parser refuses a value with the
    /// removal text (the other flags refuse `x` as not a number or a path, in other words).
    fn refusing_flags() -> BTreeSet<String> {
        let command = crate::frontend::command();
        command
            .get_arguments()
            .filter_map(|arg| arg.get_long())
            .filter(|long| {
                command
                    .clone()
                    .try_get_matches_from(["frontend", &format!("--{long}=x")])
                    .is_err_and(|e| e.to_string().contains("was removed"))
            })
            .map(str::to_owned)
            .collect()
    }

    /// The accessor names exactly the flags whose parser refuses a value, each once, with a reason
    /// that says it is removed and what to do.
    #[test]
    fn names_exactly_the_flags_that_refuse() {
        let listed: Vec<&str> = flags().map(|(name, _)| name).collect();
        let unique: BTreeSet<String> = listed.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(listed.len(), unique.len(), "a flag is listed twice");
        assert_eq!(listed.len(), 21);
        assert_eq!(unique, refusing_flags());
        assert!(flags().all(|(_, why)| why.contains("removed")));
        assert!(flags().all(|(_, why)| why.contains("delete")));
    }
}
