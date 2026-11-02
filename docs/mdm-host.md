# mdm-host: the MDM host layer

`mdm-host` is the MDM for enrolling Macs as compute hosts. It runs NanoHUB, a SCEP server,
NanoDEP and Caddy as capped systemd services on one Linux host. It belongs with the update
coordinator (the coordinator tracks every device and updates the software on it; MDM is the
Apple channel for that) and is a binary of its own, never part of the `kbf` binary. Three
crates, none of which links another crate of this workspace
(`crates/kbf-integration-test/tests/mdm_layering.rs` keeps it so):

| crate | kind | holds |
|---|---|---|
| `mdm-core` | pure | settings, units, profiles, pins: bytes from settings text, no I/O |
| `host-step` | runtime | the planner of host steps (file, directory, user, unit, command), the real host and a fake |
| `mdm-host` | binary | the binary `mdm-host`: `fetch`, `install`, `tls`, `verify` |

The settings file (`crates/mdm-core/config.example` documents every key) holds no secret and
no site name; `mdm-host` hard-codes nothing of a site. The example leaves `MDM_BIND` and
`MDM_LAN_CIDR` empty: give your site's private LAN address and network.

The names `mdm-host` puts on a host are still the ones it had before it moved here: the
`js-mdm` defaults of `config.example` (the unit prefix, the user, the slice, the directories,
the ADE organisation text, the SCEP CA's and DEP certificate's names, the signature
namespace), the comments of the rendered units, and the `.js-host.tmp` suffix of a file being
written. A host installed before the move therefore verifies `same`. They change together, in
a later change, when the coordinator's product has its name.

## Where it lives, and what comes next

The crates came from the jobscheduler repository (`js-mdm-core`, `js-host-step`, `js-mdm`) as a
move: the same code, tests and rendered files, with only the names of the crates and the program
changed. The package boundaries and the rules that keep them:

* `mdm-core` renders; it does no I/O and reaches no runtime, network or random source.
* `host-step` is the only code that touches the host; it knows nothing of MDM.
* `mdm-host` joins the two and is the only binary. Only it links the other two crates.
* None of the three links another crate of this workspace, in any kind of dependency, optional
  ones included, and neither `kbf-server` nor the `kbf` binary links them
  (`crates/kbf-integration-test/tests/mdm_layering.rs`). That keeps them ready to leave this
  repository with the update coordinator and the generic updater.

What exists beside them, and what is left to do:

* jobscheduler still holds its copy until a change there removes it and points its docs here.
* kbf-deploy's `mdm` kit is the older installer `mdm-core` was first ported from; it also has
  `push-cert`, `dep` and `erase-request`. The two install the same services, and the kit is
  folded into `mdm-host` (or removed) in a later change. Until then, use one tool per host.
* Wiring the MDM into the update coordinator is a later design step.
* The on-disk `js-mdm` names change together, once the coordinator's product has its name.
* Defects and test gaps that came over unchanged with the move are listed in
  komira-ai/kbf#2248; each is fixed in a change of its own, with a test that fails first.

## Verbs

Every verb that changes the host prints its plan by default (`DRY-RUN:` lines naming paths,
modes, owners and sizes, never contents) and acts with `--apply`. A second run finds
`nothing to do`. Exit 0 done, 1 a step, download or check failed, 2 refused (`REFUSED: ...` on
stderr).

* `mdm-host fetch --dir DIR [--only TOOL]... [--apply]`: the pinned archives, downloaded over
  HTTPS (redirects must stay on `https://`) as your own user, never root, into a directory
  outside every git work tree. Each is checked against its sha256 pin; a download that is not the
  pin is written to `<file>.rejected` and fails the run.
* `mdm-host install --config FILE --bin-dir DIR [--apply]`: root. Creates the service user, the
  directories (state 0700, etc 0750, bin 0755), unpacks the binaries from the checked archives
  (the old one kept as `<name>.previous`), makes the three secrets once from 32 random bytes in
  0600 root files (never printed, never on a command line), makes the SCEP CA unless `ca.pem`
  is there, writes the slice, the units and the Caddyfile as `mdm-core` renders them, and
  starts the services. The proxy waits for the TLS certificate.

  Operator paths must not cross a symlinked directory: `--config`, `--bin-dir` and the
  archives in it, and `MDM_STATE`, `MDM_ETC`, `MDM_BIN` and everything below them. The host
  layer opens every directory on the way without following links, because the service user
  owns directories under `MDM_STATE` and could otherwise point root at any directory of the
  host (to chmod, chown, write or read there). A refusal names the component, for example
  `refused: /etc/js-mdm/x: js-mdm is a symbolic link; give the real path`; give the real path
  instead. A directory on the way owned by a user other than root, the effective user or the
  owner the step asks for is refused too, naming the directory and the uid. `fetch --dir` is
  the one path that is resolved through links first (it is your own path, and the git work tree
  check needs the real one).
* `mdm-host install --config FILE --uninstall [--apply]`: root. Stops and disables the units and
  moves their files to `MDM_STATE/removed/<unix seconds>/`. The state, the TLS certificate, the
  SCEP CA, the secrets, the binaries and the user stay.
* `mdm-host tls --config FILE [--apply]`: root. lego over DNS-01 with the configured provider
  (GoDaddy, Route 53 or Cloudflare). The provider's credentials are lines of the root-only
  `MDM_ETC/dns.env`, which the certificate unit loads itself; they are never read into this
  program's output or any argument list. Only the checks before the plan refuse without
  changing anything: the plan runs in two parts (the proxy can be planned only once the
  certificate is there), so when the certificate is missing after the issuing unit ran, the run
  fails with exit 1 and the units and the first issuance attempt stay in place. Fix the cause
  and run it again.
* `mdm-host verify --config FILE [--ignore TEXT]...`: read only. Health checks (units active,
  listeners, certificate lifetime, modes), then one line per installed file against the render
  of the same settings:

  ```
  same       /etc/systemd/system/js-mdm-scep.service
  differs    /etc/systemd/system/js-mdm-proxy.service: installed sha256 ..., rendered sha256 ...
  missing    /etc/systemd/system/js-mdm-cert.timer
  ```

  `wrong` is printed for a file that is not a regular file (a link is never followed) or does not
  have the mode and owner `install` gives it, whatever its text. Comment lines are left out of
  both sides before hashing. `--ignore LINE` leaves out, from both sides, the lines equal to
  LINE, whole (a line that holds LINE and more is kept; an empty LINE or one with a line break
  is refused), and every line left out is printed under its file as `ignored`. A host
  installed by another tool that words one line differently is compared with one `--ignore` for
  its wording and one for the render's. A file this user may not read is `unreadable`;
  world-readable files need no root, the checks of root-only files do. A unit an admin has
  masked (a link to `/dev/null`) is reported as `wrong`, a link, not as `same` or `missing`.

## Not in this slice

`push-cert` (the APNs push certificate and the enrollment profile), `dep` (Apple Business
Manager) and `erase`, and the checks that dial the proxy over HTTPS. The enrollment profile is
therefore not yet written or compared. Security notes for the profile are in `mdm-core`'s
crate documentation.

## Tests

Tests run on `host_step::fake::FakeHost`: a file tree in memory and a command runner that
answers only the lines a test registered (anything else is an error, and every line is
recorded). The real host is tested under a scratch directory standing in for `/`.
