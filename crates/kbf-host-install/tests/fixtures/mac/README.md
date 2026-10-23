# Real output of the tools the Mac install reads

Each file is the unedited output of a tool on a GitHub-hosted macOS runner (macOS 15.7, arm64),
captured by `tests/real_mac.rs` in the `host-install-mac` workflow
(`.github/workflows/host-install-mac.yml`) and downloaded from the run's artifact
`host-install-mac-raw-output`. The first run to capture all of them is
https://github.com/komira-ai/kbf/actions/runs/39952603959. The throwaway job labels carry the
test process's id (`com.example.kbf-real-mac-test.<scenario>.<pid>`); nothing else was changed.

The crate's unit tests (`src/mac/tests/real_output.rs`) read these files, and the fake `launchctl`,
`lsof` and `codesign` of its other tests answer with these same texts.

| file | command | state |
|---|---|---|
| `print_idle.stdout` | `launchctl print system/<label>` | loaded, not started (`state = not running`, `last exit code = (never exited)`) |
| `print_running.stdout` | the same | running (`state = running`, a `pid`) |
| `print_clean_exit.stdout` | the same | ended, exit code 0 |
| `print_nonzero_exit.stdout` | the same | ended, exit code 1 |
| `print_killed.stdout` | the same | killed by `kill -9` (`last terminating signal = Killed: 9`, no `last exit code`) |
| `print_nested.stdout` | the same | loaded, not started, with `event triggers`, `endpoints` and `event channels` blocks |
| `print_missing.stderr`, `.code` | the same | label not loaded: status 113 |
| `lsof_running.stdout` | `lsof -nPw -a -p <pid> -d txt -F n` | a running process |
| `codesign_describe_hardened.stderr` | `codesign -d --verbose=4` | signed ad hoc with `--options runtime` (stdout is empty) |
| `codesign_describe_plain.stderr` | the same | signed ad hoc without it |
| `dscl_nobody.stdout` | `dscl . -read` of the `nobody` account's `UniqueID` and `PrimaryGroupID` | ids are `-2` |
| `xattr_quarantined.stdout`, `xattr_clean.stdout` | `xattr <file>` | with and without `com.apple.quarantine` |
