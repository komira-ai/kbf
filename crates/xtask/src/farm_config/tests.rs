use kbf_testkit::dir::TestDir;

use super::*;

const WHOLE: &str = "\
[buck2_re_client]
  engine_address = grpc://farm.example:8980
  action_cache_address = grpc://farm.example:8980
  cas_address = grpc://farm.example:8980
  instance_name = main
  tls = true
  execution_concurrency_limit = 256
  grpc_keepalive_timeout_secs = 60
  max_concurrent_uploads_per_action = 4
[buildrules_re]
  linux_x86_64_properties = OSFamily=Linux,ISA=x86-64
[komira_ci]
  farm_wait_seconds = 600
";

const PORT: &str = "the port is not a number from 0 to 65535";
const HOST: &str = "the host is empty, or holds whitespace or a `/`";
const SCHEME: &str = "the scheme before `://` is not a scheme";

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("xtask-farm-config-{name}"))
}

fn args(out: &Path) -> Args {
    Args {
        address: "grpc://farm.example:8980".to_owned(),
        instance: "main".to_owned(),
        tls: "true".to_owned(),
        properties: "OSFamily=Linux,ISA=x86-64".to_owned(),
        out: out.to_owned(),
    }
}

/// The fake resolver of these tests: `farm.example` is two addresses, every other name none, so
/// no test asks a real resolver.
fn resolve(host: &str, _port: u16) -> Vec<std::net::IpAddr> {
    match host {
        "farm.example" => vec![
            "192.0.2.9".parse().expect("ip"),
            "2001:db8::9".parse().expect("ip"),
        ],
        _ => Vec::new(),
    }
}

/// What `farm.example` adds to the masks: the bare host and each address, alone and with the port.
const RESOLVED: &str = "::add-mask::farm.example\n::add-mask::192.0.2.9\n::add-mask::192.0.2.9:8980\n\
                        ::add-mask::2001:db8::9\n::add-mask::[2001:db8::9]:8980\n";

/// The result of `run_with` for `args`, and the lines it printed.
fn run_of(args: &Args) -> (Result<String, Error>, String) {
    let mut printed = Vec::new();
    let result = run_with(args, &mut printed, &resolve);
    (result, String::from_utf8(printed).expect("utf-8"))
}

/// A log that refuses every write.
struct Refusing;

impl Write for Refusing {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("the log is closed"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn the_address_is_masked_with_and_without_its_scheme() {
    // Catches: an address that reaches the log unmasked, a scheme-less form left unmasked, a
    // mask for a blank address (an empty `::add-mask::`), and a doubled mask when there is no
    // scheme.
    assert_eq!(
        masks("grpc://farm.example:8980"),
        [
            "::add-mask::grpc://farm.example:8980",
            "::add-mask::farm.example:8980"
        ]
    );
    assert_eq!(masks(" 192.0.2.1:80 "), ["::add-mask::192.0.2.1:80"]);
    assert_eq!(masks("grpc://"), ["::add-mask::grpc://"]);
    assert_eq!(masks(""), Vec::<String>::new());
    assert_eq!(masks("  "), Vec::<String>::new());
}

#[test]
fn the_buckconfig_is_exactly_the_farms_keys_and_the_worker_properties_under_buildrules() {
    // Catches: a key renamed or dropped, the properties under another section, the TLS flag
    // rendered as anything but true or false, the address not given to all three services.
    let dir = scratch("whole");
    let out = dir.join("config");
    let (result, printed) = run_of(&args(&out));
    assert_eq!(result, Ok(format!("farm-config: wrote {}", out.display())));
    assert_eq!(
        printed,
        format!("::add-mask::grpc://farm.example:8980\n::add-mask::farm.example:8980\n{RESOLVED}")
    );
    assert_eq!(std::fs::read_to_string(&out).expect("read"), WHOLE);

    let mut plain = args(&out);
    plain.tls = "false".to_owned();
    plain.address = "farm.example:8980".to_owned();
    assert!(run_of(&plain).0.is_ok());
    let text = std::fs::read_to_string(&out).expect("read");
    assert_eq!(
        text,
        WHOLE
            .replace("grpc://", "")
            .replace("tls = true", "tls = false")
    );
}

#[test]
fn surrounding_whitespace_is_not_written() {
    // Catches: a value padded by a workflow's variable written with its padding.
    let dir = scratch("trimmed");
    let out = dir.join("config");
    let mut padded = args(&out);
    padded.address = "  grpc://farm.example:8980 ".to_owned();
    padded.instance = " main\t".to_owned();
    padded.properties = " OSFamily=Linux,ISA=x86-64  ".to_owned();
    assert!(run_of(&padded).0.is_ok());
    assert_eq!(std::fs::read_to_string(&out).expect("read"), WHOLE);
}

#[test]
fn the_directory_of_the_file_is_made_when_missing() {
    // Catches: a write that fails because `~/.buckconfig.d` does not exist yet.
    let dir = scratch("nested");
    let out = dir.join("a").join("b").join("farm.buckconfig");
    assert!(run_of(&args(&out)).0.is_ok());
    assert_eq!(std::fs::read_to_string(&out).expect("read"), WHOLE);
}

#[test]
fn a_file_replaces_what_was_there() {
    // Catches: a config appended to an earlier one, or a longer earlier file left after the new
    // one's end.
    let dir = scratch("replace");
    let out = dir.join("config");
    std::fs::write(&out, "x".repeat(WHOLE.len() * 2)).expect("earlier");
    assert!(run_of(&args(&out)).0.is_ok());
    assert_eq!(std::fs::read_to_string(&out).expect("read"), WHOLE);
}

#[test]
fn a_blank_address_instance_or_properties_is_refused_naming_its_flag_and_writes_nothing() {
    // Catches: a fork's run (no variables) building on the runner, and a refusal that does not
    // say which value is missing.
    let dir = scratch("blank");
    let out = dir.join("config");
    for blank in ["", "   ", "\t"] {
        for flag in ["address", "instance", "properties"] {
            let mut a = args(&out);
            match flag {
                "address" => a.address = blank.to_owned(),
                "instance" => a.instance = blank.to_owned(),
                _ => a.properties = blank.to_owned(),
            }
            let (result, printed) = run_of(&a);
            assert_eq!(result, Err(Error::Blank { flag }), "{flag} {blank:?}");
            let words = format!(
                "--{flag} is empty: a fork's run gets no variables; refusing, because buck2 would \
                 build on the runner"
            );
            assert_eq!(result.expect_err("refused").to_string(), words);
            if flag == "address" {
                assert_eq!(printed, "", "nothing to mask in a blank value");
            }
            assert!(!out.exists());
        }
    }
}

#[test]
fn a_value_with_a_line_break_is_refused_naming_its_flag() {
    // Catches: a newline or carriage return in a value starting another line of the file (a key
    // of the attacker's choosing), in any of the three values, trailing or inside.
    let dir = scratch("breaks");
    let out = dir.join("config");
    for value in ["a\nb", "a\rb", "a\r\nb", "x\n"] {
        for flag in ["address", "instance", "properties"] {
            let mut a = args(&out);
            match flag {
                "address" => a.address = format!("farm.example:80{value}"),
                "instance" => a.instance = value.to_owned(),
                _ => a.properties = value.to_owned(),
            }
            let (result, _) = run_of(&a);
            assert_eq!(result, Err(Error::LineBreak { flag }), "{flag} {value:?}");
            assert_eq!(
                result.expect_err("refused").to_string(),
                format!("--{flag} holds a line break: each value is one line of the buckconfig")
            );
            assert!(!out.exists());
        }
    }
}

#[test]
fn tls_must_be_exactly_true_or_false() {
    // Catches: `True`, `1`, `yes`, an empty value or a padded one taken for a TLS setting.
    let dir = scratch("tls");
    let out = dir.join("config");
    for tls in ["", "True", "FALSE", "1", "yes", " true", "true ", "true\n"] {
        let mut a = args(&out);
        a.tls = tls.to_owned();
        let (result, _) = run_of(&a);
        let error = result.expect_err("refused");
        assert_eq!(error, Error::Tls(tls.to_owned()));
        assert_eq!(
            error.to_string(),
            format!("--tls is {tls:?}: it must be exactly `true` or `false`")
        );
        assert!(!out.exists());
    }
}

#[test]
fn an_address_that_is_not_host_and_numeric_port_is_refused() {
    // Catches: an address with no port, an empty or non-numeric or too large port, an empty
    // host, a host with a space or a path, and a scheme that is not one.
    let dir = scratch("address");
    let out = dir.join("config");
    let cases = [
        ("farm.example", "it has no `:port`"),
        ("grpc://farm.example", "it has no `:port`"),
        ("farm.example:", PORT),
        ("farm.example:grpc", PORT),
        ("farm.example:+80", PORT),
        ("farm.example:65536", PORT),
        (":8980", HOST),
        ("grpc://:8980", HOST),
        ("far m:8980", HOST),
        ("grpc://farm.example/path:8980", HOST),
        ("1grpc://farm.example:8980", SCHEME),
        ("://farm.example:8980", SCHEME),
        ("gr pc://farm.example:8980", SCHEME),
    ];
    for (address, why) in cases {
        let mut a = args(&out);
        a.address = address.to_owned();
        let (result, printed) = run_of(&a);
        let error = result.expect_err("refused");
        assert_eq!(error, Error::Address { why }, "{address}");
        assert_eq!(
            error.to_string(),
            format!(
                "--address is not host:port, optionally behind a scheme such as grpc://: {why}"
            )
        );
        assert!(printed.starts_with("::add-mask::"), "masked before refused");
        assert!(!out.exists());
    }
}

#[test]
fn addresses_with_and_without_schemes_and_odd_hosts_pass() {
    // Catches: a valid address refused: every scheme shape, an IP, a bracketed IPv6 host, the
    // port's bounds.
    for address in [
        "farm:1",
        "grpc://farm:8980",
        "grpcs://farm.example:443",
        "http://192.0.2.7:80",
        "https+x.y-z://farm:65535",
        "[fd7a::1]:8980",
        "farm:0",
    ] {
        let mut a = args(Path::new("unused"));
        a.address = address.to_owned();
        assert_eq!(
            check(&a).map(|s| s.address.to_owned()),
            Ok(address.to_owned())
        );
    }
}

#[test]
fn the_first_refused_flag_in_order_is_the_one_named() {
    // Catches: a refusal that hides the first problem behind a later one.
    let mut a = args(Path::new("unused"));
    a.address = String::new();
    a.tls = "x".to_owned();
    assert_eq!(check(&a), Err(Error::Blank { flag: "address" }));
    a.address = "farm:1".to_owned();
    a.instance = String::new();
    assert_eq!(check(&a), Err(Error::Blank { flag: "instance" }));
    a.instance = "main".to_owned();
    assert_eq!(check(&a), Err(Error::Tls("x".to_owned())));
    a.tls = "false".to_owned();
    a.properties = String::new();
    assert_eq!(check(&a), Err(Error::Blank { flag: "properties" }));
}

#[test]
fn a_file_that_cannot_be_written_is_refused_naming_it() {
    // Catches: a file the job could not write reported as written, a directory that cannot be
    // made (a file where it should be) read as made, and an empty path.
    let dir = scratch("unwritable");
    let as_directory = dir.join("is-a-directory");
    std::fs::create_dir(&as_directory).expect("directory");
    let (result, _) = run_of(&args(&as_directory));
    assert_eq!(
        result.expect_err("refused").to_string(),
        format!("cannot write {as_directory:?}: Is a directory (os error 21)")
    );

    let (result, _) = run_of(&args(Path::new("")));
    assert_eq!(
        result.expect_err("refused").to_string(),
        "cannot write \"\": No such file or directory (os error 2)"
    );

    let file = dir.join("file");
    std::fs::write(&file, "x").expect("file");
    let below = file.join("config");
    let (result, _) = run_of(&args(&below));
    assert_eq!(
        result.expect_err("refused").to_string(),
        format!("cannot write {below:?}: File exists (os error 17)")
    );
}

#[test]
fn a_log_that_cannot_be_written_does_not_stop_the_file() {
    // Catches: a closed log failing the verb.
    let dir = scratch("log-closed");
    let out = dir.join("config");
    assert!(run_with(&args(&out), &mut Refusing, &resolve).is_ok());
    assert_eq!(std::fs::read_to_string(&out).expect("read"), WHOLE);
}

fn settings<'a>(address: &'a str, host: &'a str, port: u16) -> Settings<'a> {
    Settings {
        address,
        host,
        port,
        instance: "main",
        tls: false,
        properties: "p",
    }
}

#[test]
fn the_host_and_every_address_it_resolves_to_are_masked_once() {
    // Catches: the resolved address that buck2's connection errors print (`tcp connect error ...
    // <ip>:<port>`) reaching the public log, the bare host left unmasked, an IPv6 address with
    // the port written without brackets, a mask repeated, and a lookup of a bracketed host.
    let address = "grpc://farm.example:8980";
    assert_eq!(
        resolved_masks(&settings(address, "farm.example", 8980), &resolve).concat(),
        RESOLVED.replace('\n', "")
    );
    // A host that does not resolve: only the host itself.
    assert_eq!(
        resolved_masks(&settings("other.example:1", "other.example", 1), &resolve),
        ["::add-mask::other.example"]
    );
    // An IP that resolves to itself repeats nothing `masks` printed (the bare address).
    let ip = |host: &str, _: u16| vec![host.parse().expect("ip")];
    assert_eq!(
        resolved_masks(&settings("192.0.2.4:80", "192.0.2.4", 80), &ip),
        ["::add-mask::192.0.2.4"]
    );
    assert_eq!(
        resolved_masks(&settings("[2001:db8::4]:80", "[2001:db8::4]", 80), &ip),
        ["::add-mask::[2001:db8::4]", "::add-mask::2001:db8::4"]
    );
    // The same address twice from the resolver is masked once.
    let twice = |_: &str, _: u16| vec!["192.0.2.6".parse().expect("ip"); 2];
    assert_eq!(
        resolved_masks(&settings("h:2", "h", 2), &twice),
        [
            "::add-mask::h",
            "::add-mask::192.0.2.6",
            "::add-mask::192.0.2.6:2"
        ]
    );
}

#[test]
fn check_hands_over_the_host_and_port_it_read() {
    // Catches: a host or port taken from the wrong side of the last `:`, and a scheme kept.
    let dir = scratch("parts");
    let mut a = args(&dir.join("config"));
    a.address = "grpcs://[2001:db8::1]:443".to_owned();
    let s = check(&a).expect("checked");
    assert_eq!((s.host, s.port), ("[2001:db8::1]", 443));
}

#[test]
fn every_line_of_an_address_with_a_line_break_is_masked_before_the_refusal() {
    // Catches: the line after a line break in the address reaching the public log unmasked (a
    // mask covers one line), and a line masked twice.
    assert_eq!(
        masks("farm.example:80\nb"),
        ["::add-mask::farm.example:80", "::add-mask::b"]
    );
    assert_eq!(
        masks("grpc://h:1\r\ngrpc://h:1\r"),
        ["::add-mask::grpc://h:1", "::add-mask::h:1"]
    );
    let dir = scratch("address-line-break");
    let mut a = args(&dir.join("config"));
    a.address = "grpc://farm.example:8980\nsecond".to_owned();
    let (result, printed) = run_of(&a);
    assert_eq!(result, Err(Error::LineBreak { flag: "address" }));
    assert_eq!(
        printed,
        "::add-mask::grpc://farm.example:8980\n::add-mask::farm.example:8980\n::add-mask::second\n"
    );
}

#[test]
fn a_lookup_gives_its_addresses_and_a_failed_one_gives_none() {
    // Catches: a failed lookup failing the verb or masking something, and an answer's addresses
    // dropped or changed.
    let answer: Vec<std::net::SocketAddr> = vec![
        "192.0.2.1:80".parse().expect("addr"),
        "[2001:db8::1]:80".parse().expect("addr"),
    ];
    assert_eq!(
        ips(Ok(answer.into_iter())),
        [
            "192.0.2.1".parse::<std::net::IpAddr>().expect("ip"),
            "2001:db8::1".parse().expect("ip")
        ]
    );
    let failed: std::io::Result<std::vec::IntoIter<std::net::SocketAddr>> =
        Err(std::io::Error::other("no such host"));
    assert_eq!(ips(failed), Vec::<std::net::IpAddr>::new());
}
