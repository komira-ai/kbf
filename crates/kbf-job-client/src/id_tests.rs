use super::*;

/// Asserts the edges of a job id: 1 and 63 bytes of every allowed kind of character are
/// taken; empty and 64 bytes are refused with their length; a space, a slash, a colon and a
/// letter outside ASCII are each refused by name. Catches an off-by-one at either length and
/// a character class wider than the design's (a slash would reach a path, a non-ASCII letter
/// would count its bytes, not its characters).
#[test]
fn a_job_id_is_1_to_63_bytes_of_letters_digits_dot_underscore_dash() {
    assert_eq!(check("a"), Ok(()));
    assert_eq!(check("Job.1_x-Z9"), Ok(()));
    assert_eq!(check(&"a".repeat(MAX_ID)), Ok(()));
    assert_eq!(check(""), Err(BadId::Length(0)));
    assert_eq!(check(&"a".repeat(MAX_ID + 1)), Err(BadId::Length(64)));
    assert_eq!(check("job 1"), Err(BadId::Character(' ')));
    assert_eq!(check("a/b"), Err(BadId::Character('/')));
    assert_eq!(check("a:b"), Err(BadId::Character(':')));
    assert_eq!(check("jöb"), Err(BadId::Character('ö')));
    assert_eq!(
        BadId::Length(64).to_string(),
        "a job id is 1 to 63 bytes, and this one is 64"
    );
    assert_eq!(
        BadId::Character('/').to_string(),
        "a job id holds letters, digits, '.', '_' and '-' only, and this one holds '/'"
    );
}
