//! The property lists the test profiles must render, written out by hand.

/// The property list the test profile must render, given the first 12 hex digits of
/// the profile's SHA-256: written out here, not rendered, so a rendering defect shows.
pub fn golden_plist(sha12: &str) -> String {
    golden_plist_as(sha12, Some("_kbf"))
}

/// The golden property list with `UserName` and `GroupName` set to `user`, or without
/// them (a job launchd runs as root) for `None`.
pub fn golden_plist_as(sha12: &str, user: Option<&str>) -> String {
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
		<string>/usr/local/kbf/current/kbf-daemon</string>
		<string>--driver</string>
		<string>native</string>
		<string>--server</string>
		<string>https://farm.example.net:8981</string>
		<string>--cas</string>
		<string>https://farm.example.net:8981</string>
		<string>--ca-cert</string>
		<string>/usr/local/kbf/etc/ca.pem</string>
		<string>--cert</string>
		<string>/usr/local/kbf/etc/node.pem</string>
		<string>--key</string>
		<string>/usr/local/kbf/etc/node.key</string>
		<string>--node-id</string>
		<string>kbf-mac-07</string>
		<string>--scratch</string>
		<string>/Volumes/kbf/scratch</string>
		<string>--label</string>
		<string>profile=mac-arm64-test@{sha12}</string>
		<string>--label</string>
		<string>pool=mac</string>
		<string>--label</string>
		<string>rack=r2</string>
	</array>
{user}	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ThrottleInterval</key>
	<integer>10</integer>
	<key>ExitTimeOut</key>
	<integer>30</integer>
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
