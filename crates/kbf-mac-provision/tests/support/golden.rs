//! The property lists the test profiles must render, written out by hand.

/// The property list the test profile must render, given the first 12 hex digits of
/// the profile's SHA-256: written out here, not rendered, so a rendering defect shows.
pub fn golden_plist(sha12: &str) -> String {
    golden_plist_as(sha12, Some("_kbf"))
}

/// The golden property list with `UserName` and `GroupName` set to `user`, or without
/// them (a job launchd runs as root) for `None`.
pub fn golden_plist_as(_sha12: &str, user: Option<&str>) -> String {
    let user = user.map_or_else(String::new, |u| {
        format!("\t<key>UserName</key>\n\t<string>{u}</string>\n\t<key>GroupName</key>\n\t<string>{u}</string>\n")
    });
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>org.example.kbf-daemon</string>
	<key>ProgramArguments</key>
	<array>
		<string>/usr/local/kbf-installer/releases/current/kbf</string>
		<string>--mode=daemon</string>
		<string>--config</string>
		<string>/usr/local/etc/kbf/kbf.toml</string>
	</array>
{user}	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ThrottleInterval</key>
	<integer>10</integer>
	<key>ExitTimeOut</key>
	<integer>2147483647</integer>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>StandardErrorPath</key>
	<string>/Library/Logs/kbf/kbf-daemon.err.log</string>
	<key>SoftResourceLimits</key>
	<dict>
		<key>NumberOfFiles</key>
		<integer>65536</integer>
	</dict>
	<key>HardResourceLimits</key>
	<dict>
		<key>NumberOfFiles</key>
		<integer>65536</integer>
	</dict>
</dict>
</plist>
"#
    )
}

/// The golden `kbf.toml` of the test profile: written by hand, not by the renderer, so a
/// key, a table or a value the renderer gets wrong differs from it. Each key is in the
/// table of kbf-cli's `GROUPS` for the flag of that name. It has no `label` key, and so
/// does not depend on the profile's SHA-256.
pub fn golden_config() -> String {
    r#"# Written by kbf-mac-provision from its profile; `apply` puts it back if it is edited.
# One table per flag group, one key per flag long name (kbf-cli, config.rs).
mode = "daemon"

[identity]
server = "https://farm.example.net:8981"
cas = "https://farm.example.net:8981"
ca-cert = "/usr/local/kbf/etc/ca.pem"
cert = "/usr/local/kbf/etc/node.pem"
key = "/usr/local/kbf/etc/node.key"
node-id = "kbf-mac-07"

[daemon]
driver = ["native"]
data-dir = "/Volumes/kbf/data"
"#
    .to_owned()
}
