//! The few DER elements the request and key parsers read.
//!
//! Strict on purpose: one tag byte, a length in its shortest form (short, `0x81` or `0x82`;
//! a request is at most 2 KiB), and an element that fits what is left. Anything else is
//! `None`, so a request has one spelling and a parser has no length it must guess.

/// One element: its content and its whole encoding.
#[derive(Clone, Copy)]
pub(crate) struct Element<'a> {
    /// The bytes after the header.
    pub content: &'a [u8],
    /// Header and content.
    pub raw: &'a [u8],
}

/// Reads one element whose tag is `tag` off the front of `input`, returning it and the
/// bytes after it.
pub(crate) fn read(input: &[u8], tag: u8) -> Option<(Element<'_>, &[u8])> {
    let (&first, rest) = input.split_first()?;
    if first != tag {
        return None;
    }
    let (len, body) = length(rest)?;
    let content = body.get(..len)?;
    let header = input.len() - body.len();
    Some((
        Element {
            content,
            raw: &input[..header + len],
        },
        &body[len..],
    ))
}

/// A DER length in its shortest form, and the bytes after it.
fn length(input: &[u8]) -> Option<(usize, &[u8])> {
    let (&first, rest) = input.split_first()?;
    match first {
        0..=0x7f => Some((usize::from(first), rest)),
        0x81 => {
            let (&n, rest) = rest.split_first()?;
            (n >= 0x80).then_some((usize::from(n), rest))
        }
        0x82 => {
            let (n, rest) = rest.split_first_chunk::<2>()?;
            let n = usize::from(u16::from_be_bytes(*n));
            (n >= 0x100).then_some((n, rest))
        }
        _ => None,
    }
}
