//! Reading `dscl`'s text output. Pure, so its tests run on every platform; what a real
//! `dscl` prints is checked by the macOS run (the spike refuses to go on when it reads
//! nothing it expects).

/// `dscl . -list /Users UniqueID` (or `/Groups PrimaryGroupID`): `name   id` per line; a
/// line with no id after the name is a record without the attribute.
pub fn parse_list(text: &str) -> Vec<(String, Option<u32>)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|line| match line.rsplit_once(char::is_whitespace) {
            Some((name, id)) => match id.parse() {
                Ok(id) => (name.trim().to_owned(), Some(id)),
                Err(_) => (line.to_owned(), None),
            },
            None => (line.to_owned(), None),
        })
        .collect()
}

/// `dscl . -list /Users`: one name per line.
pub fn parse_names(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `dscl . -read record A B`: `A: value` on a line of its own, or `A:` then the value on
/// the next, indented line (what dscl does with a value that has a space in it). Only the
/// `wanted` attributes, with `dsAttrTypeNative:` stripped, in the order printed.
pub fn parse_attrs(text: &str, wanted: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let line = line.strip_prefix("dsAttrTypeNative:").unwrap_or(line);
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        if !wanted.contains(&key) {
            continue;
        }
        let inline = rest.trim();
        let value = if inline.is_empty() {
            lines
                .peek()
                .filter(|next| next.starts_with(char::is_whitespace))
                .map(|next| next.trim())
        } else {
            Some(inline)
        };
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            out.push((key.to_owned(), value.to_owned()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_line_is_a_name_and_its_id() {
        let text = "_amavisd                 83\nroot                     0\nodd                      \nkbf-scale-3              61003\n";
        assert_eq!(
            parse_list(text),
            vec![
                ("_amavisd".to_owned(), Some(83)),
                ("root".to_owned(), Some(0)),
                ("odd".to_owned(), None),
                ("kbf-scale-3".to_owned(), Some(61003)),
            ]
        );
    }

    #[test]
    fn a_name_with_no_id_and_blank_lines_are_names_only() {
        assert_eq!(parse_list("\nsolo\n\n"), vec![("solo".to_owned(), None)]);
        assert_eq!(parse_names("a\n\n  b  \n"), vec!["a", "b"]);
    }

    #[test]
    fn an_inline_value_and_one_on_the_next_line_both_read() {
        let text = "NFSHomeDirectory: /private/var/kbf-scale-homes.noindex/kbf-scale-3\nRealName:\n kbf-scale-spike-mark\n";
        assert_eq!(
            parse_attrs(text, &["RealName", "NFSHomeDirectory"]),
            vec![
                (
                    "NFSHomeDirectory".to_owned(),
                    "/private/var/kbf-scale-homes.noindex/kbf-scale-3".to_owned()
                ),
                ("RealName".to_owned(), "kbf-scale-spike-mark".to_owned()),
            ]
        );
    }

    #[test]
    fn an_attribute_not_asked_for_or_without_a_value_or_native_prefixed() {
        let text =
            "Password: *\nRealName:\nUserShell: /usr/bin/false\ndsAttrTypeNative:IsHidden: 1\n";
        assert_eq!(
            parse_attrs(text, &["RealName", "IsHidden"]),
            vec![("IsHidden".to_owned(), "1".to_owned())]
        );
    }

    #[test]
    fn an_indented_continuation_of_another_attribute_is_not_a_value() {
        let text = "AuthenticationAuthority:\n ;ShadowHash;\nRealName: x\n";
        assert_eq!(
            parse_attrs(text, &["RealName"]),
            vec![("RealName".to_owned(), "x".to_owned())]
        );
    }
}
