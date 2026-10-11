//! The shapes a profile value must have. Each function says which pattern it accepts,
//! as an extended regular expression matched against the whole value.

fn lower(b: u8) -> bool {
    b.is_ascii_lowercase()
}

fn digit(b: u8) -> bool {
    b.is_ascii_digit()
}

fn alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

/// Reads a value from the left, one class of byte at a time.
struct Cursor<'a> {
    rest: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn new(s: &'a str) -> Self {
        Self { rest: s.as_bytes() }
    }

    /// Takes the bytes of `class` from the left; how many it took.
    fn many(&mut self, class: impl Fn(u8) -> bool) -> usize {
        let n = self.rest.iter().take_while(|&&b| class(b)).count();
        self.rest = &self.rest[n..];
        n
    }

    /// Takes one byte of `class`, if the next is one.
    fn one(&mut self, class: impl Fn(u8) -> bool) -> bool {
        match self.rest.split_first() {
            Some((&b, rest)) if class(b) => {
                self.rest = rest;
                true
            }
            _ => false,
        }
    }

    /// Takes `text`, if the value continues with it.
    fn literal(&mut self, text: &str) -> bool {
        match self.rest.strip_prefix(text.as_bytes()) {
            Some(rest) => {
                self.rest = rest;
                true
            }
            None => false,
        }
    }

    fn done(&self) -> bool {
        self.rest.is_empty()
    }
}

/// `[0-9]+`
pub fn number(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(digit)
}

/// `[a-z0-9][a-z0-9._-]*`
pub fn profile_name(s: &str) -> bool {
    let mut c = Cursor::new(s);
    c.one(|b| lower(b) || digit(b)) && {
        c.many(|b| lower(b) || digit(b) || b"._-".contains(&b));
        c.done()
    }
}

/// `[0-9]+(\.[0-9]+)*`
pub fn version(s: &str) -> bool {
    !s.is_empty() && s.split('.').all(number)
}

/// `[0-9]+[A-Z][0-9]+[a-z]?`: a macOS or Xcode build.
pub fn build(s: &str) -> bool {
    let mut c = Cursor::new(s);
    let shaped = c.many(digit) > 0 && c.one(|b| b.is_ascii_uppercase()) && c.many(digit) > 0;
    c.one(lower);
    shaped && c.done()
}

/// `[a-z][a-z0-9]*(-[a-z0-9]+)*-mac-[0-9]+`
pub fn node_name(s: &str) -> bool {
    let Some((farm, n)) = s.rsplit_once("-mac-") else {
        return false;
    };
    let mut parts = farm.split('-');
    let first = parts.next().unwrap_or_default();
    number(n)
        && first.as_bytes().first().is_some_and(|&b| lower(b))
        && first.bytes().all(|b| lower(b) || digit(b))
        && parts.all(|p| !p.is_empty() && p.bytes().all(|b| lower(b) || digit(b)))
}

/// `[a-z][a-z0-9._-]*`
pub fn account(s: &str) -> bool {
    let mut c = Cursor::new(s);
    c.one(lower) && {
        c.many(|b| lower(b) || digit(b) || b"._-".contains(&b));
        c.done()
    }
}

/// `_[a-z][a-z0-9_]*`
pub fn role_account(s: &str) -> bool {
    let mut c = Cursor::new(s);
    c.literal("_") && c.one(lower) && {
        c.many(|b| lower(b) || digit(b) || b == b'_');
        c.done()
    }
}

/// `[0-1]`
pub fn bit(s: &str) -> bool {
    s == "0" || s == "1"
}

/// `[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?`
pub fn host_name(s: &str) -> bool {
    let b = s.as_bytes();
    b.first().is_some_and(|&f| alnum(f))
        && b.last().is_some_and(|&l| alnum(l))
        && b.iter().all(|&x| alnum(x) || x == b'.' || x == b'-')
}

/// `[A-Za-z0-9][A-Za-z0-9.-]*`
pub fn launchd_label(s: &str) -> bool {
    let mut c = Cursor::new(s);
    c.one(alnum) && {
        c.many(|b| alnum(b) || b == b'.' || b == b'-');
        c.done()
    }
}

/// `https://[A-Za-z0-9.-]+(:[0-9]+)?`
pub fn https_url(s: &str) -> bool {
    let mut c = Cursor::new(s);
    c.literal("https://")
        && c.many(|b| alnum(b) || b == b'.' || b == b'-') > 0
        && (c.done() || (c.literal(":") && c.many(digit) > 0 && c.done()))
}

/// `/[A-Za-z0-9._/-]+`
pub fn absolute_path(s: &str) -> bool {
    let mut c = Cursor::new(s);
    c.literal("/") && c.many(|b| alnum(b) || b"._/-".contains(&b)) > 0 && c.done()
}

/// `[A-Za-z0-9._-]+=[A-Za-z0-9._:/@+,=-]+`: one of the daemon's `--label`s.
pub fn daemon_label(s: &str) -> bool {
    s.split_once('=').is_some_and(|(k, v)| {
        !k.is_empty()
            && k.bytes().all(|b| alnum(b) || b"._-".contains(&b))
            && !v.is_empty()
            && v.bytes().all(|b| alnum(b) || b"._:/@+,=-".contains(&b))
    })
}

/// `[0-9a-f]{64}`
pub fn sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| digit(b) || (b'a'..=b'f').contains(&b))
}

/// `/Applications/Xcode[A-Za-z0-9._-]*\.app/Contents/Developer`
pub fn xcode_developer_dir(s: &str) -> bool {
    s.strip_prefix("/Applications/Xcode")
        .and_then(|rest| rest.strip_suffix(".app/Contents/Developer"))
        .is_some_and(|middle| middle.bytes().all(|b| alnum(b) || b"._-".contains(&b)))
}

/// `[a-z][a-z0-9_]*`: a probe's key.
pub fn probe_key(s: &str) -> bool {
    let mut c = Cursor::new(s);
    c.one(lower) && {
        c.many(|b| lower(b) || digit(b) || b == b'_');
        c.done()
    }
}

/// `[A-Za-z0-9._:+-]*`: what a probe is expected to print.
pub fn probe_value(s: &str) -> bool {
    s.bytes().all(|b| alnum(b) || b"._:+-".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pattern, a value, and whether the pattern accepts it.
    type Case<'a> = (fn(&str) -> bool, &'a str, bool);

    /// Catches: a pattern that accepts a value its expression refuses, or refuses one
    /// it accepts, at either end of the value.
    #[test]
    fn each_pattern_accepts_its_shape_and_nothing_else() {
        let cases: &[Case<'_>] = &[
            (number, "0480", true),
            (number, "", false),
            (number, "4a", false),
            (profile_name, "mac-arm64.pool_1", true),
            (profile_name, "9pool", true),
            (profile_name, "", false),
            (profile_name, "-pool", false),
            (profile_name, "Mac Pool", false),
            (profile_name, "pool!", false),
            (version, "27.0.1", true),
            (version, "27", true),
            (version, "27.x", false),
            (version, "27.", false),
            (version, "", false),
            (build, "26A000", true),
            (build, "26A000b", true),
            (build, "26a000", false),
            (build, "A000", false),
            (build, "26A", false),
            (build, "26A000bc", false),
            (node_name, "kbf-mac-07", true),
            (node_name, "kbf2-west-mac-1", true),
            (node_name, "a-mac-1-mac-2", true),
            (node_name, "kbf-07", false),
            (node_name, "Kbf-mac-07", false),
            (node_name, "2kbf-mac-07", false),
            (node_name, "-mac-07", false),
            (node_name, "kbf--x-mac-07", false),
            (node_name, "kbf-mac-", false),
            (node_name, "kbf-mac-7a", false),
            (node_name, "kbf_x-mac-7", false),
            (account, "farmadmin", true),
            (account, "a.b_c-d9", true),
            (account, "Admin", false),
            (account, "", false),
            (account, "a b", false),
            (role_account, "_kbf_2", true),
            (role_account, "kbf", false),
            (role_account, "_", false),
            (role_account, "_kbf-x", false),
            (bit, "0", true),
            (bit, "1", true),
            (bit, "2", false),
            (host_name, "time.example.net", true),
            (host_name, "a", true),
            (host_name, "-x", false),
            (host_name, "x-", false),
            (host_name, "a_b", false),
            (host_name, "", false),
            (launchd_label, "org.example.kbf-daemon", true),
            (launchd_label, ".x", false),
            (launchd_label, "a/b", false),
            (https_url, "https://farm.example.net:8981", true),
            (https_url, "https://farm.example.net", true),
            (https_url, "http://farm.example.net", false),
            (https_url, "https://", false),
            (https_url, "https://farm.example.net/x", false),
            (https_url, "https://farm.example.net:", false),
            (https_url, "https://farm.example.net:89x", false),
            (absolute_path, "/usr/local/kbf/etc/ca.pem", true),
            (absolute_path, "/", false),
            (absolute_path, "node.pem", false),
            (absolute_path, "/a b", false),
            (daemon_label, "pool=mac", true),
            (daemon_label, "a=b=c", true),
            (daemon_label, "rack", false),
            (daemon_label, "=mac", false),
            (daemon_label, "pool=", false),
            (daemon_label, "po ol=mac", false),
            (daemon_label, "pool=m*c", false),
            (sha256, &"ab".repeat(32), true),
            (sha256, &"AB".repeat(32), false),
            (sha256, &"a".repeat(63), false),
            (sha256, &"g".repeat(64), false),
            (
                xcode_developer_dir,
                "/Applications/Xcode-27.0.app/Contents/Developer",
                true,
            ),
            (
                xcode_developer_dir,
                "/Applications/Xcode.app/Contents/Developer",
                true,
            ),
            (
                xcode_developer_dir,
                "/opt/Xcode.app/Contents/Developer",
                false,
            ),
            (
                xcode_developer_dir,
                "/Applications/Xcode/x.app/Contents/Developer",
                false,
            ),
            (xcode_developer_dir, "/Applications/Xcode.app", false),
            (probe_key, "host_identity2", true),
            (probe_key, "Host", false),
            (probe_key, "host-id", false),
            (probe_value, "", true),
            (probe_value, "26.6-0123456789abcdef", true),
            (probe_value, "a=b", false),
        ];
        // The values whose verdict is wrong (each row says which verdict it wants).
        let wrong: Vec<(&str, bool)> = cases
            .iter()
            .filter_map(|&(f, s, want)| (f(s) != want).then_some((s, want)))
            .collect();
        assert!(wrong.is_empty(), "{wrong:#?}");
    }
}
