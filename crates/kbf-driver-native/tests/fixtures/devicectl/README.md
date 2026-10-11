# devicectl fixtures

What `xcrun devicectl` prints, kept as test input for the device survey parser
(docs/design/ios-devices.md, sections 11 and 12).

- **P-D0** (no device: `devicectl`'s help, `--version` and an empty
  `list devices --json-output` for each Xcode on a hosted macOS runner): not
  captured yet.
- **P-D1** (a real device and a person at it): not captured yet.

Until then this README is the only file here.

Every file in this directory is read by the repository lint
`devicectl_fixtures_carry_no_device_identifiers` (`crates/kbf-it/tests/repo_lints.rs`,
rules in `crates/kbf-it/tests/device_ids/mod.rs`). It refuses a real-looking UDID,
ECID, serial number, CoreDevice id or device name, and a `.json` file that does not
parse. Scrub a fixture to these placeholders before committing it:

| Identifier | Placeholder |
|---|---|
| UDID | `00000000-0000000000000000` (or 40 zeros for the older form) |
| CoreDevice id | `00000000-0000-0000-0000-000000000000` |
| ECID | `0` |
| Serial number | `XXXXXXXXXXXX` |
| Device name | `Device Name` |

The lint cannot see an identifier glued to other letters or digits, a serial number
or ECID with no label next to it, or a device name outside the two JSON keys it
checks; read a P-D1 fixture before committing it.
