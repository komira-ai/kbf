//! The `Range` header of a blob request: one byte range.

use hyper::header::HeaderValue;

/// What a `Range` header asks of an object of a known size.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Range {
    /// The whole object: no header, one this front does not understand (more than one range,
    /// another unit, a malformed range), which a server may ignore.
    Whole,
    /// Bytes `start..=end` of the object, both inside it.
    Partial { start: u64, end: u64 },
    /// A range that holds no byte of the object.
    Unsatisfiable,
}

/// The range `header` asks of an object of `size` bytes: `bytes=a-b`, `bytes=a-` or `bytes=-n`.
pub(super) fn parse(header: Option<&HeaderValue>, size: u64) -> Range {
    let spec = header
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.strip_prefix("bytes="))
        .filter(|spec| !spec.contains(','))
        .and_then(|spec| spec.split_once('-'));
    let Some((first, last)) = spec else {
        return Range::Whole;
    };
    if first.is_empty() {
        return suffix(last, size);
    }
    let Ok(start) = first.parse::<u64>() else {
        return Range::Whole;
    };
    let end = if last.is_empty() {
        Ok(u64::MAX)
    } else {
        last.parse::<u64>()
    };
    match end {
        Ok(end) if end < start => Range::Whole,
        Ok(_) if start >= size => Range::Unsatisfiable,
        Ok(end) => Range::Partial {
            start,
            end: end.min(size - 1),
        },
        Err(_) => Range::Whole,
    }
}

/// The last `count` bytes of an object of `size` bytes.
fn suffix(count: &str, size: u64) -> Range {
    match count.parse::<u64>() {
        Err(_) => Range::Whole,
        Ok(0) => Range::Unsatisfiable,
        Ok(_) if size == 0 => Range::Unsatisfiable,
        Ok(count) => Range::Partial {
            start: size.saturating_sub(count),
            end: size - 1,
        },
    }
}
