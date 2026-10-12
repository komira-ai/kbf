# XCUITest fixtures

Two tiny SwiftUI apps, each with a UI test target. The macOS probes use them as their
instrument: a UI test that is known to work, so a probe that runs it in a new context
(another user, another session, under a sandbox profile) learns about the context and
not about the test.

- `MacFixture.xcodeproj`: a macOS app and `MacFixtureUITests`.
- `IOSFixture.xcodeproj`: an iOS app and `IOSFixtureUITests`, tested on a simulator.

Both projects compile the same sources:

| file | in | what it does |
| --- | --- | --- |
| `App/FixtureApp.swift`, `App/ContentView.swift` | apps | one window: a four-colour pattern, a label (`ready`, then `tapped <n>`) and a button |
| `Common/Pattern.swift` | apps, tests, selftest | the pattern's colours and the launch argument that leaves it out |
| `UITests/FixtureUITests.swift` | tests | the three UI tests below |
| `UITests/PatternCheck.swift` | tests, selftest | looks for the pattern in a screenshot |
| `selftest/main.swift` | selftest | the checker against synthetic images |

Nothing is signed with an identity. macOS builds are signed to run locally (ad-hoc,
`CODE_SIGN_IDENTITY = "-"`, no team). The iOS project lists both iOS platforms
(`SUPPORTED_PLATFORMS = "iphonesimulator iphoneos"`), but only a simulator build runs
without a team, and the tests ask only for a simulator destination.

## The tests

| test | proves | turns red when |
| --- | --- | --- |
| `testTapChangesLabel` | a tap reaches the app and the new label is read back through accessibility | the tap is lost, or the app sets another label |
| `testScreenshotShowsPattern` | the window screenshot holds the painted pattern; the whole-screen screenshot is attached and its verdict printed, not asserted | the screenshot is black, wallpaper or another app's window |
| `testPatternLeftOutIsNotFound` | the control arm: launched with `-kbf-no-pattern`, the checker finds no pattern | the checker passes any screenshot |

Each test prints a `KBF-FIXTURE <name>=PASS|FAIL <detail>` line and keeps every
screenshot in the `.xcresult` (attachment lifetime `keepAlways`), whatever the outcome.

`PatternCheck` draws the image into an 8-bit sRGB bitmap. Pattern colours can occur
elsewhere on a screen, so it anchors on each solid, roughly square block of the
top-left colour and finds the pattern only where the three same-sized boxes beside it
(right, below, below-right) hold the other three colours. The selftest checks it
against a pattern on a light and on a dark background, and a pattern among other
pattern-coloured pixels (each found), and against an all-black image, a wallpaper of
hue bands, the quadrants in the wrong order, three quadrants and the quadrants
scattered apart (each missing).

## Running

`crates/kbf-driver-native/tests/xcuitest_sandbox.rs` builds and tests the fixtures
(macOS with Xcode and an iOS simulator runtime only, so its tests are `#[ignore]`d):

```sh
cargo test -p kbf-driver-native --test xcuitest_sandbox --locked -- --ignored --nocapture
```

It runs the pixel checker's selftest (`xcrun swiftc` of `selftest/main.swift` with the
two sources it checks), each fixture without a sandbox (the controls), and each under
the native driver's sandbox profile as the driver runs an action
(`docs/spikes/macos-p0.md`). The iOS fixture runs on the first iPhone of the newest
iOS runtime `simctl` lists; the macOS one in this login session.
`tests/support/xcode_fixture.rs` holds the `xcodebuild` command and the check of the
result bundle: a run passes only if xcodebuild exited 0, exactly the three tests
passed and the exported attachments hold the three screenshots (`window` and `screen`
from `testScreenshotShowsPattern`, `window-no-pattern` from
`testPatternLeftOutIsNotFound`), each a non-empty file. Inside the bundle the
attachments are content-addressed files under `Data/`; `xcresulttool export
attachments` writes them out with a `manifest.json` that names the test and attachment
each file came from. A test that passes without `keepAlways` has its screenshots
deleted from the bundle, so that run fails.

Each run leaves its lease under `target/tmp/xcuitest/<arm>`: the
`<platform>.xcresult` bundle, the xcodebuild log and the exported attachments in
`root/out`, and the run's deny lines in `target/tmp/xcuitest/<arm>-denies.txt`. The
workflow `.github/workflows/spike-xcuitest.yml` runs the test on a hosted macOS runner
and uploads them. A hosted runner's privacy grants may be seeded, so its result proves
the instrument, not what a fresh Mac allows.

## Changing the projects

The `.xcodeproj` files are the source: edit them in Xcode (or by hand) and commit
them. They were first written by a generator, which was deleted once the projects
stopped changing; the test builds both projects on every run, so a project that no
longer parses fails it.
