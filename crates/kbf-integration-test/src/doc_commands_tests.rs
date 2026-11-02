use super::*;

/// The commands of `doc` as `(line, info, words joined by one space)`.
fn read(doc: &str) -> Vec<(usize, String, String)> {
    cluster_commands(doc)
        .expect("commands")
        .into_iter()
        .map(|c| (c.line, c.info, c.argv.join(" ")))
        .collect()
}

/// The words of the one command in a `text` block holding `line`.
fn words(line: &str) -> Vec<String> {
    let got = cluster_commands(&format!("```text\n{line}\n```\n")).expect("commands");
    assert_eq!(got.len(), 1, "{line}: {got:?}");
    got.into_iter().next().map(|c| c.argv).unwrap_or_default()
}

#[test]
fn commands_are_read_from_fenced_blocks_only_with_their_info_and_line() {
    // Catches: prose read as a command, a block's info string lost, a continued command
    // read as two or placed on its last line, a comment line read, tilde fences missed.
    let doc = "\
Run `kbf cluster status` first.

```text
kbf cluster drain <node> --token-file /etc/kbf/api-token
sudo /usr/bin/kbf cluster object-store repoint --connection <connection> \\
    --endpoint <new endpoint URL> --expect-generation <generation>
# kbf cluster not-a-command
```

~~~shell
run kbf cluster --api \"$API\" undrain \"$node_id\" --token-file \"${TOKEN_FILE}\"
~~~
";
    assert_eq!(
        read(doc),
        [
            (
                4,
                "text".to_owned(),
                "kbf cluster drain node-1 --token-file /etc/kbf/api-token".to_owned()
            ),
            (
                5,
                "text".to_owned(),
                "kbf cluster object-store repoint --connection cold --endpoint \
                 http://store-b.example --expect-generation 7"
                    .to_owned()
            ),
            (
                11,
                "shell".to_owned(),
                "kbf cluster --api 127.0.0.1:8982 undrain node-1 --token-file \
                 /etc/kbf/api-token"
                    .to_owned()
            ),
        ]
    );
}

#[test]
fn whitespace_quotes_and_a_continuation_between_the_words_still_make_a_command() {
    // Catches: a command missed (and so never parsed) because of a tab or two spaces
    // between `kbf` and `cluster`, a quoted path to kbf, or a `\` between the words.
    assert_eq!(
        words("kbf \t cluster  status"),
        ["kbf", "cluster", "status"]
    );
    assert_eq!(
        words("\"/usr/bin/kbf\" cluster status"),
        ["kbf", "cluster", "status"]
    );
    let doc = "```\nkbf \\\n  cluster status\n```\n";
    assert_eq!(
        read(doc),
        [(2, String::new(), "kbf cluster status".to_owned())]
    );
    // Not commands: another program, another word, a line a script only prints.
    let doc = "```\nmykbf cluster status\nkbf clusters\necho \"+ kbf cluster drain x\"\n```\n";
    assert_eq!(read(doc), []);
}

#[test]
fn the_words_after_a_redirection_stay_in_the_command() {
    // Catches: a misspelled flag after `>out` or `2>&1` dropped from the check although
    // the shell passes it to kbf, a descriptor read as a word, a redirection target read
    // as an argument.
    assert_eq!(
        words("kbf cluster drain n >out --bad"),
        ["kbf", "cluster", "drain", "n", "--bad"]
    );
    assert_eq!(
        words("kbf cluster drain n 2>&1 --bad"),
        ["kbf", "cluster", "drain", "n", "--bad"]
    );
    assert_eq!(
        words("kbf cluster drain n 2> err --bad &>all"),
        ["kbf", "cluster", "drain", "n", "--bad"]
    );
    assert_eq!(
        words("kbf cluster drain n >&out --bad"),
        ["kbf", "cluster", "drain", "n", "--bad"]
    );
    assert_eq!(
        words("kbf cluster drain n < in"),
        ["kbf", "cluster", "drain", "n"]
    );
    // Digits that end a longer word, or are quoted, are a word, not a descriptor.
    assert_eq!(
        words("kbf cluster drain node9>out"),
        ["kbf", "cluster", "drain", "node9"]
    );
    assert_eq!(
        words("kbf cluster drain \"2\">out"),
        ["kbf", "cluster", "drain", "2"]
    );
}

#[test]
fn quoted_metacharacters_and_spaces_stay_inside_their_word() {
    // Catches: a quoted `;` or `&` cutting the command short so later flags go unchecked,
    // and a quoted value split into two words.
    assert_eq!(
        words("kbf cluster drain 'a;b&c' --x"),
        ["kbf", "cluster", "drain", "a;b&c", "--x"]
    );
    assert_eq!(
        words("kbf cluster drain \"a b|c\" --x"),
        ["kbf", "cluster", "drain", "a b|c", "--x"]
    );
    assert_eq!(
        words("kbf cluster drain a\\ b \"q\\\"t\" \"\""),
        ["kbf", "cluster", "drain", "a b", "q\"t", ""]
    );
    assert_eq!(
        words("kbf cluster drain \"c\\d\""),
        ["kbf", "cluster", "drain", "c\\d"]
    );
}

#[test]
fn a_hash_inside_a_word_is_part_of_it_and_starts_a_comment_only_at_a_word_start() {
    // Catches: `a#b` cut at the `#`, so the rest of the command goes unchecked.
    assert_eq!(
        words("kbf cluster drain a#b --x #c --y"),
        ["kbf", "cluster", "drain", "a#b", "--x"]
    );
}

#[test]
fn control_operators_and_substitutions_end_a_command() {
    // Catches: two commands read as one, or a command inside `$(...)` (quoted or not)
    // missed, or a word after `)` glued to it.
    let doc = "```shell\n\
x=\"$(kbf cluster status | jq .nodes)\"; kbf cluster member $member_id --state active && y\n\
[ \"$(kbf cluster status)\" = drained ] || (kbf cluster undrain a) ; kbf cluster x ;; z\n\
a ) kbf cluster y # kbf cluster comment\n\
```\n";
    let got: Vec<String> = read(doc).into_iter().map(|(_, _, c)| c).collect();
    assert_eq!(
        got,
        [
            "kbf cluster status",
            "kbf cluster member 1 --state active",
            "kbf cluster status",
            "kbf cluster undrain a",
            "kbf cluster x",
            "kbf cluster y",
        ]
    );
}

#[test]
fn what_cannot_be_read_is_an_error_naming_its_line() {
    // Catches: a command that escapes the check because the reader gave up on it
    // silently: an unknown value, a backtick, an open quote, a continued line cut by
    // its fence, a fence that never closes.
    let err = |doc: &str| cluster_commands(doc).expect_err(doc);
    assert_eq!(
        err("```\nkbf cluster drain <who>\n```\n"),
        CommandError::Placeholder {
            line: 2,
            name: "<who>".into()
        }
    );
    assert_eq!(
        err("```\nkbf cluster drain <open\n```\n"),
        CommandError::Placeholder {
            line: 2,
            name: "<open".into()
        }
    );
    for (line, name) in [
        ("kbf cluster drain \"$WHO\"", "WHO"),
        ("kbf cluster drain $1", "1"),
        ("kbf cluster drain $", ""),
        ("kbf cluster drain ${API", "API"),
        ("kbf cluster drain $9API", "9API"),
    ] {
        assert_eq!(
            err(&format!("```\n{line}\n```\n")),
            CommandError::Variable {
                line: 2,
                name: name.into()
            },
            "{line}"
        );
    }
    for line in [
        "kbf cluster object-store status --token-file f >\"$(printf out)\" --misspelled",
        "kbf cluster drain \"$(echo n)\" --bad",
        "kbf cluster drain n --x \"a$(echo b)\"",
    ] {
        assert_eq!(
            err(&format!("```\n{line}\n```\n")),
            CommandError::Unsupported {
                line: 2,
                what: "a command substitution inside a kbf cluster command"
            },
            "{line}"
        );
    }
    assert_eq!(
        err("```\nkbf cluster drain `x`\n```\n"),
        CommandError::Unsupported {
            line: 2,
            what: "a backtick"
        }
    );
    for line in [
        "kbf cluster drain 'x",
        "kbf cluster drain \"x",
        "(kbf cluster drain x",
    ] {
        assert_eq!(
            err(&format!("```\n{line}\n```\n")),
            CommandError::Unclosed(2),
            "{line}"
        );
    }
    assert_eq!(
        err("```\nok\nkbf cluster drain \\\n```\n"),
        CommandError::Unfinished(3)
    );
    assert_eq!(
        err("text\n```shell\nkbf cluster x\n"),
        CommandError::Fence(2)
    );
}

#[test]
fn a_fence_closes_only_with_its_own_marker_alone_on_its_line() {
    // Catches: a `~~~` or a longer or info-bearing line closing a ``` block early (the
    // rest of the block then read as prose and never checked).
    let doc = "````text\n~~~\n```\nkbf cluster a\n```x\n`````\nkbf cluster prose\n";
    assert_eq!(
        read(doc),
        [(4, "text".to_owned(), "kbf cluster a".to_owned())]
    );
    // Two backticks are not a fence.
    assert_eq!(read("``\nkbf cluster a\n``\n"), []);
}

#[test]
fn every_error_says_its_line_and_what_is_wrong() {
    // Catches: an error message that names no line, so a doc author cannot find it.
    let cases = [
        (
            CommandError::Placeholder {
                line: 3,
                name: "<who>".into(),
            },
            "line 3: placeholder <who> has no sample value in PLACEHOLDERS",
        ),
        (
            CommandError::Variable {
                line: 4,
                name: "WHO".into(),
            },
            "line 4: shell variable $WHO has no sample value in SHELL_VARIABLES",
        ),
        (
            CommandError::Unfinished(5),
            "line 5: a command continued with `\\` has no next line in its block",
        ),
        (
            CommandError::Unclosed(6),
            "line 6: a quote or parenthesis never closes",
        ),
        (
            CommandError::Fence(7),
            "line 7: the fenced block never closes",
        ),
        (
            CommandError::Unsupported {
                line: 8,
                what: "a backtick",
            },
            "line 8: a backtick is not read",
        ),
    ];
    for (e, text) in cases {
        assert_eq!(e.to_string(), text);
    }
}
