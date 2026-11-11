use super::*;

#[test]
fn render_then_parse_gives_the_fields_back_with_their_continuation_lines() {
    // Catches: a multi-line Description losing a line or its leading space on the way through.
    let fields = [
        Field::new("Package", "kbf"),
        Field::new("Description", "short\n long line\n .\n more"),
    ];
    let text = render(&fields).expect("render");
    assert_eq!(
        text,
        "Package: kbf\nDescription: short\n long line\n .\n more\n"
    );
    let got = parse(&text).expect("parse");
    assert_eq!(got, fields);
    assert_eq!(
        get(&got, "Description"),
        Some("short\n long line\n .\n more")
    );
    assert_eq!(get(&got, "Nope"), None);
    assert_eq!(render(&got).expect("again"), text);
}

#[test]
fn a_field_cannot_carry_a_second_field_a_nul_or_an_empty_value() {
    // Catches: a value that forges a field (`Depends`) or a name that is not a name.
    for (name, value) in [
        ("Package", "kbf\nDepends: evil"),
        ("Package", "mo\0pm"),
        ("Package", "a\rb"),
        ("Package", ""),
        ("Pack age", "x"),
        ("", "x"),
        ("Package", "kbf\n"),
    ] {
        assert!(
            render(&[Field::new(name, value)]).is_err(),
            "{name:?} {value:?}"
        );
    }
}

#[test]
fn text_that_is_not_a_stanza_is_refused() {
    // Catches: a continuation line with nothing to continue, and a line with no colon, read as
    // fields.
    assert!(
        parse(" orphan\n")
            .unwrap_err()
            .to_string()
            .contains("starts with a continuation line")
    );
    assert!(
        parse("Package kbf\n")
            .unwrap_err()
            .to_string()
            .contains("without a colon")
    );
    assert_eq!(parse("").expect("empty"), []);
    // A tab also continues a line.
    assert_eq!(parse("A: 1\n\tmore\n").expect("tab")[0].value, "1\n\tmore");
}

#[test]
fn stanzas_are_split_on_blank_lines() {
    // Catches: stanzas run together, an empty one, or the last one lost when the text does not
    // end in a newline.
    assert_eq!(
        split_stanzas("A: 1\nB: 2\n\nA: 3\n\n\nA: 4\n"),
        ["A: 1\nB: 2\n", "A: 3\n", "A: 4\n"]
    );
    assert_eq!(split_stanzas("A: 1\n\nA: 2"), ["A: 1\n", "A: 2"]);
    assert!(split_stanzas("").is_empty());
    assert!(split_stanzas("\n\n").is_empty());
}
