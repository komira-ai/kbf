# Fleet: provisioning and operating the hardware

This document says how an operator brings up and keeps a farm of about 3 server and
storage nodes, about 20 Linux build hosts and about 6 Mac Studios. It is a design:
**planned** unless a section says "today". The operator learns three things: one
inventory file (`fleet.toml`), one operator command (`kbf-deploy`) and one place to look
(`kbf-deploy status`). Behind those, `kbf-deploy` installs a list of components
(section 2); the operator needs to know them only when something breaks.

It builds on [fleet-updates.md](fleet-updates.md) (rollouts),
[fleet-updates-security.md](fleet-updates-security.md) (threat model, signing),
[mdm-backend.md](mdm-backend.md) (Mac MDM), [mac-node-provisioning.md](mac-node-provisioning.md)
(Mac host settings) and [daemon.md](daemon.md).

## 1. Summary

| Question | Answer |
|---|---|
| How many tools? | One operator CLI, `kbf-deploy`, with the verbs in section 9 (not fewer verbs than today: one binary, one inventory, one concept set instead of five kits and shell scripts). It installs and configures the components in section 2. The three product binaries (`kbf-server`, `kbf-daemon`, `kbf-all-in-one`) are unchanged. |
| How many config files? | One: `fleet.toml` in a git repo the operator owns. Every unit file, launchd plist, certificate request and PXE answer is rendered from it. Nobody edits a unit by hand. |
| What identifies a machine? | The **node name** typed once in the inventory and written into the machine's certificate. A hardware id read from firmware is the lookup key that maps a box to its name; it is not a proof (section 3, section 4). Reinstalling or replacing a disk changes nothing. |
| How does Linux get an OS? | PXE/iPXE netboot of an Ubuntu installer, answered by an autoinstall seed rendered per machine from the inventory. A host installs only when the operator **arms** it (section 5.1); otherwise netboot sends it to its local disk. |
| How does a Mac get set up? | Apple Business Manager (ABM) assigns the serial to the farm's MDM. The MDM pushes a signed `.pkg`; a launchd job it installs enrolls the node. |
| How do nodes get configured later? | Nodes pull their own signed slice of a spec from the farm's object store. The operator publishes with `kbf-deploy apply`. |
| How do nodes update? | The same spec names the software set. `kbf-deploy rollout` runs canary, soak and rollback in batches. |
| What does the operator do physically? | Section 5.5 is the baseline and section 12 says what stays manual. It is more than "rack and power on": BIOS baseline, a provisioning VLAN, matching boxes to names, the Apple accounts, and a console for the first Mac. |

Principles: the inventory is the only place a fact is typed (name, role, label, address);
everything else is derived from hardware or from the inventory. Flags, not environment
variables, in every rendered unit. Secrets never rest on a build host except that host's
own node key.

### What `install.sh` is, and what replaces it

`install.sh` (Mac daemon installer) and `install_units.sh` (Linux converged-node
installer) are shell scripts that copy a binary, write a unit or plist and start it, each
with its own copy of the node id, labels and server address. They are why the same facts
are typed in several places. They are replaced by `kbf-deploy node apply`, which renders
the same files from the inventory (Linux: run by the first-boot unit; Mac: run by a
launchd job the `.pkg` installs). Section 10 lists every removal.

## 2. Components and who installs them

The honest claim is "one operator CLI, N components it installs". An operator must know
what each is and where it runs.

| Component | Runs on | Installed by | Operator touches it when |
|---|---|---|---|
| `kbf-server` | server hosts | `node apply` from the spec | never directly |
| `kbf-ca` (enrollment signer; own user and socket, section 4.1) | server hosts | `node apply` | root ceremony, intermediate renewal |
| MinIO (distributed over the three servers) | server hosts | `node apply` | a drive fails (section 11.2) |
| envoy (mTLS gate in front of the object store) | server hosts | `node apply` | never directly |
| netboot service (proxyDHCP, TFTP, HTTPS: `kbf-deploy netboot serve`) | one or more server hosts (`services = ["netboot"]`) | `node apply` | arming, adopting hosts |
| MDM: NanoHUB, NanoDEP, SCEP, `kbf-mdm-gate` | the host(s) with `services = ["mdm"]`, see 5.2 | `node apply` | ABM tasks, annual renewals |
| `kbf-daemon` / `kbf-all-in-one` | builders / all-in-one hosts | `node apply` | never directly |
| node agent (`kbf-updater` on Linux; macOS agent does not exist yet, section 13) | every host | first-boot unit / `.pkg` | never directly |
| Outside kbf: DHCP, DNS, NTP, switch and VLAN, BIOS, ABM, Apple Developer ID | site | the operator | section 5.5 |

Which services a server host runs is a list in its inventory block (`services`), so
"where is the MDM installed" is answered by `kbf-deploy status`.

## 3. What exists today, and the gap

Facts checked against the code:

- `kbf-deploy` is a Linux-only binary with five kits (`minio`, `linux`, `mdm`, `envoy`,
  `pilot`) and about 20 verbs. `kbf-deploy linux` always installs both server and daemon
  and expects MinIO as a systemd unit; there is no daemon-only profile.
- `kbf-mac-provision` checks and applies Mac host settings and renders the LaunchDaemon.
  It installs no binary and makes no certificate.
- `kbf-mdm-gate`, NanoHUB, SCEP and NanoDEP exist (`kbf-deploy mdm`).
- `kbf-updater` (Linux only) installs signed OS software sets. **A macOS node agent
  does not exist.**
- `RolloutDriver` stops at the `updating` step; its store is in memory; no node-side
  wiring exists. Nothing rolls out real software today.
- `--node-id` is a typed string. The server requires the client certificate to carry
  exactly one DNS name equal to it. The node registry lives in memory.
- Certificates come from a shell script. There is no enrollment endpoint, rotation or
  revocation except a hand-edited deny-list.
- No PXE, netboot or autoinstall exists. Linux hosts are installed by hand.
- Server HA: the design describes three servers (ha.md, open); the code has one server
  with a single-voter metadata log.

The sprawl is a result of there being no inventory.

## 4. Persistent identity, enrollment and trust

### 4.1 Authorities and keys

- **Fleet root** (ECDSA P-256). Created once by `kbf-deploy ca init` on an air-gapped
  machine or a hardware key; the private key never touches a networked host. It signs
  only: intermediates, delegated spec keys, release keys, and the revocation list's
  signer. Signing is offline: the online side produces a request file, the operator
  carries it on removable media, `kbf-deploy ca sign` (run on the offline machine)
  returns the signed file.
- **Two roots.** Every node trusts a set of two roots from install: the current root and
  a standby root generated at the same ceremony and stored in a second location.
  Rotating the root is: sign the standby into use through a spec signed by the current
  root, then retire the old. A lost or stolen root is recovered by promoting the standby.
  The root-rotation drill is run once before production and written in the runbook.
- **Farm CA intermediates**: one per server host, each generated on its host by the
  `kbf-ca` process (a separate user and socket from `kbf-server`, so the enrollment
  listener cannot read the key), signed by the root offline. Key files are mode 0600,
  owned by `kbf-ca`; on hardware with a TPM the key is TPM-sealed. Each intermediate is
  **name-constrained**: DNS suffix of the fleet and `clientAuth` EKU only, so it can sign
  neither servers nor other CAs. Lifetime one year; `ca rotate` issues a new request,
  the new intermediate arrives in the next spec, and nodes accept any intermediate that
  chains to a trusted root. No key is replicated; compromise of one server revokes that
  server's intermediate (root-signed) and nothing else.
- **Spec keys**: delegated, one per operator, each signed by the root with an expiry (one
  year) that nodes enforce, each held on that operator's hardware token. Several
  operators therefore sign independently and every spec records which key signed it.
  Losing one token revokes one delegated key.
- **Release key**: held by CI, signed by the root. The node verifies the binary's own
  release-key signature, **and** that its hash equals the hash in the spec. Either
  failing refuses the install. Release-key compromise is handled as a root-signed
  replacement key in the next spec.
- **No rollback**: the spec has a monotonic version and each release a monotonic
  sequence number. A node refuses a spec or release below what it already runs. Going
  back to older software is a new spec (higher version) naming the older release by a
  rollback entry signed by an operator.

What a compromised server host can do is stated, not hidden: with root on a server an
attacker can make that server's intermediate issue certificates for any node name in the
inventory (the name constraint limits it to fleet names), and so impersonate a build
host and read inputs and the CAS. It cannot push software (it holds no spec or release
key). Detection and containment: every issuance is in the audit log (4.8); revoke the
intermediate with a root-signed entry. This is an accepted risk (CEO decision 5).

### 4.2 The hardware id is a lookup key, not a proof

| Platform | Key | Fallback | Why not the usual |
|---|---|---|---|
| Linux | `/sys/class/dmi/id/product_uuid` as `dmi:<uuid>` | `board_serial`, then `chassis_serial` | `/etc/machine-id` changes on reinstall. MAC addresses change with a NIC swap and are shared by bonds. |
| Mac | the serial number as `mac:<serial>` | none | The serial is what ABM and the MDM know. `IOPlatformUUID` is recorded as a second check, not a key. |

The hwid is printed on labels and readable by anyone with root, so it proves nothing:
the **bootstrap token** (4.4) or the **MDM identity** (4.5) is the only proof of
identity, and `/enroll` never accepts a hwid alone. A spoofed DMI value on a host that
holds a valid token is the residual risk; a TPM attestation (EK or AK bound to the CSR)
would close it and is deferred to open items.

Rules:

- Lower-cased; a placeholder (all zeros, all `f`, `To Be Filled By O.E.M.`,
  `Default string`, `Not Specified`) is refused at adoption, not at enrollment; the
  operator then sets the id from the board label and the BIOS vendor tool writes it.
- **Byte order and case.** The SMBIOS UUID is mixed-endian; iPXE `${uuid}` and the kernel
  `product_uuid` must agree. The netboot service canonicalizes both, and a purchase
  acceptance test on **one box of each hardware model** boots it through iPXE and the
  installer and compares the two. Without that test the model is not accepted.
- **Duplicates** (cheap boards, cloned OEM batches): `adopt` (4.3) refuses a hwid already
  bound, and `/enroll` refuses a hwid that another live node reports. Both are caught
  before the field, not as a surprise 409.
- A virtual machine used by kbf's own CI may set `hwid = "static:<name>"` with
  `trust = "test"`. Test hosts are issued certificates from a separate intermediate
  whose name constraint is `*.test.<suffix>`, so a test host cannot be given a
  production name by mistake.

`kbf-deploy hwid` prints the id of the machine it runs on.

### 4.3 From a box in a rack to a name (discovery)

The problem: twenty identical servers power on and `status` shows twenty hwids. The
chassis label shows the vendor **serial**, not the `product_uuid`. The method:

1. The vendor ships a serial list with the purchase order. The operator imports it:
   `kbf-deploy add --from-csv hosts.csv` (columns: serial, name, profile). Each host
   block then has `serial = "..."` and no `hwid`. Racking puts the name label on the
   chassis next to the serial the vendor already printed.
2. On first PXE contact the netboot service sees `${serial}` and `${uuid}` and records
   them as a **pending match**. `kbf-deploy status` shows
   `build-007: serial S seen at <ip>, mac <mac>, uuid U: adopt?`.
3. `kbf-deploy adopt build-007` (or `adopt --all-pending`, which lists every match
   first and asks once) writes `hwid` into the inventory, commits it, and **arms** the
   host (4.4). The operator checks the line against what they racked.

For small batches `kbf-deploy add build-009 --adopt-next` binds whichever unknown
machine appears next and is used one box at a time. Switch-port or LLDP mapping would let
the switch say which slot a box is in; it is not in the first cut.

Reports about unknown machines are rate-limited and capped (256 entries, oldest dropped)
so a hostile host on the VLAN cannot fill `status`. Unknown hwids and serials are shown
only to the authenticated operator, never to the caller.

### 4.4 Linux bootstrap: arming, token, enrollment

Netboot's default answer to any machine is **boot the local disk**. A host installs only
while **armed**: `kbf-deploy adopt` arms a new host, and `kbf-deploy reinstall <name>`
arms an existing one. Arming is one-shot and expires (2 hours). Because of that,
UEFI network boot can be first in the boot order, a power loss never reinstalls a
MinIO node, and a stray reboot does nothing.

The bootstrap token is minted **at arming**, stored in the durable registry (4.7), and
delivered once:

1. The installer fetches `GET /seed/<hwid>` over TLS pinned to the root (the iPXE image
   embeds the root; CI builds it). The first fetch for an armed host returns the seed and
   consumes the arming; any later fetch is a 409 and an alert.
2. The token is inside that seed and is written by the install to a root-only file
   (mode 0600). It is passed to `kbf-deploy node enroll` as a **path**, never in argv and
   never as a long-lived value in the served user-data after the fetch.
3. Consumption at `/enroll` is an atomic compare-and-set in the durable store. The
   endpoint does not start until that store exists (step 0 in section 13).

If a real host loses a race (409 on its seed), the operator sees the alert and the 409
host id, re-arms, and the alert shows the first fetcher's address, so an attacker's
fetch is visible. This does not make the VLAN safe: **the provisioning VLAN is a trust
boundary** (5.5). Anyone who can reach netboot while a host is armed and who knows its
hwid can race for the seed. The controls are a short armed window, the operator
arming only when about to power the host, port security on the switch and `status`
showing which address fetched. Optionally `arm --mac <mac>` pins the NIC.

After the install: the late-command only writes files (the `kbf-deploy` binary fetched
with a signature check against the embedded root, the token file, a unit) and enables
`kbf-firstboot.service`, a oneshot that retries with backoff after network and time sync.
Enrollment and starting services happen there, not in the installer (no systemd runs in
the installer chroot).

Autoinstall failure: the seed sets `error-commands` that POST the failing step and log tail
to the netboot service (`/report/<hwid>`) and then **power off** (not reboot: the
host must not loop). `status <name> --why` reads those reports, so a host without a
certificate still has a reason on record.

Re-enrollment after expiry or a lost key: `kbf-deploy enroll-token <name>` (or
`--pool <p>` or `--all`) prints a fresh token valid 1 hour over the operator's
authenticated channel. A token printed this way is also single-use, consumed by CAS and
passed by file path.

### 4.5 The enrollment endpoint and Mac evidence

`POST /enroll` on a second listener of `kbf-server` (not the worker port). It is the one
unauthenticated surface in a mutual-TLS farm, so it binds to the provisioning VLAN and the
farm network only, is rate-limited per source, and refuses with one uniform 403 to any caller
that fails evidence (so name probing and hwid probing tell an attacker nothing). It takes
`{hwid, name, csr, evidence}` and:

1. looks up `hwid`; the name must equal the inventory name;
2. verifies the evidence: a token (Linux, 4.4) or the MDM identity (Mac, below);
3. checks the live-identity rule: an already-registered node under another key is refused
   until the operator runs `retire` or `replace`. There is **no automatic takeover**: a
   crashed original is never replaced by a clone without an operator action;
4. signs the CSR, one DNS name equal to the node name, 30-day lifetime, records the
   serial in the audit log (4.8) and returns the chain.

Renewal uses the same endpoint over mTLS with the current certificate, at **half** of the
lifetime (day 15). A node can therefore be cut off from the servers for 15 days without
losing its identity. After that `enroll-token --all` is the recovery path; the runbook
says so (section 11.4).

**Clocks.** Every host runs NTP against a source the profile names (`chrony` rendered on
Linux, the OS time service on Mac); certificate validation tolerates 5 minutes of skew;
`status` flags a host whose Hello time differs from the server's by more.

**Mac evidence.** The Mac enrolls in the MDM first (5.2). The MDM's SCEP profile gives
it a device identity whose subject carries the serial. At `/enroll` the node presents that
certificate (mTLS); the server checks that the serial equals the inventory `mac:<serial>`,
and asks `kbf-mdm-gate` whether the device is still enrolled, **supervised** and not
quarantined. A manually enrolled, unsupervised Mac (not bought through ABM) can be
unenrolled by its local admin; it gets `trust = "test"` and a test-only intermediate.

State of trust on the Mac side, in plain words: the MDM operator can enroll any device
and push any profile or package, so a compromised MDM is a compromised Mac fleet. The
gate controls only the server's path to the MDM, not the reverse. Mitigations: the
`.pkg` payload carries the farm's own signature, which the postinstall verifies against the
root embedded in the package before anything runs (a notarized package proves only that
Apple scanned it); an ABM assignment is not enough (the inventory entry is the second
factor, so a serial added in ABM but absent from the inventory never receives a farm
certificate); changes to the ABM assignment list alert. The key is generated on the host
and stored in a root-owned file in the System keychain; "never leaves the host" is a
policy of the code, not a hardware guarantee, and Secure Enclave storage is an open item.

### 4.6 Revocation

`kbf-deploy retire <name>` removes the host from the inventory, **revokes its certificate
by serial** (never by name or hwid, so retiring and re-adding the same hwid within 30 days
does not revive the old certificate), and for a Mac asks the gate to unenroll the device.

The revocation list is a signed, versioned object in the durable registry and
the object store. `kbf-server` and envoy both consult it. Server rule: it keeps the last good
list on disk; if the object store is unreachable it keeps using that list and alerts;
it refuses to start if it has never had a list (fail closed at start, fail static after).
Nodes hold no object-store or CAS credentials (they reach the store through envoy with
their certificate), so retire needs nothing else to cut.

### 4.7 The durable registry

The node registry, arming records, token state and revocation list must survive a server
restart: otherwise a restart un-revokes everyone and resets "used" bits. **This is a
prerequisite, not an open item**: `/enroll` is not enabled until it exists. Before HA it
is a file store on the single server (write, fsync, rename), copied to the object store
every few minutes and to the operator on `kbf-deploy backup pull`; with HA it rides on the
metadata log.

### 4.8 Audit

Every enroll, renew, retire, arm, token mint, spec publish and ABM change is appended as an
object under `fleet/audit/<date>/` in the object store (object lock if the store supports
it) with the acting key id. `status --audit` reads it.

### 4.9 Manual duties

Section 12 lists what the operator still does by hand.

## 5. Provisioning the operating system

### 5.1 Linux hosts: PXE with arming

```
power on -> UEFI PXE -> DHCP (proxyDHCP from netboot) -> iPXE (HTTPS, root embedded)
  -> not armed: "boot local disk".   armed: kernel+initrd of the pinned Ubuntu installer
  -> installer fetches  GET /seed/<hwid>  (iPXE passes ${uuid} ${serial}); consumed once
  -> autoinstall: os_disk, network, admin key, apt snapshot, late-command, error-commands
  -> reboot -> kbf-firstboot.service: verify binary, enroll, node apply
```

Where the inventory comes from: `netboot serve` loads the **signed spec bundle** that
`apply` produces, from `--spec-url` (the object store, once it exists) or `--spec-dir`
(a directory; day 0 and cold start). It verifies the spec key and caches the bundle on
disk. The same bundle is the only inventory source for every tool, so there is one
mechanism. When the operator runs `add` and `apply`, the netboot host sees the new spec
at its next poll (a minute); the operator does not copy anything. Pending-match and
failure reports from netboot to `status` go the other way: `status` queries each netboot
instance listed in `[fleet].netboot` on its authenticated admin port (operator
certificate), which is how a netboot on a different host is reached.

The seed renders: hostname = name; storage from `disks`, where **`os_disk` is an explicit
by-id path** and is the only disk the installer may touch; `disks.minio` is never in the
installer's storage config. The installer refuses to start if `os_disk` is absent from
the host's block (no guess). Network comes from the profile (DHCP; servers static);
apt is the pinned snapshot `kbf-updater` uses. The installer needs outbound access or an
apt mirror (prerequisites, 5.5).

Why PXE and autoinstall, not an image: the seed is rendered from the inventory on every
install, so there is no image to rebuild or drift. A bootc image remains the long-term
target (fleet-updates.md) and replaces only the installer step.

Fallback without PXE: `kbf-deploy netboot usb <name>` writes the installer and seed for one
host to a USB stick. The stick contains the armed token, so it is one-shot and
expires like any arming.

### 5.2 Macs: ABM and the MDM

1. The Mac is bought or added in ABM under the farm's MDM server. `kbf-deploy mac assign
   <serial>` registers the serial with NanoDEP.
2. Rack, ethernet (not Wi-Fi), power on. Setup Assistant contacts Apple, is told the MDM
   and enrolls. **The first Mac needs a display and keyboard** to create the local admin
   and approve the volume owner. Every later Mac, if its ABM profile skips the panes,
   does not. This is unmeasured [A].
3. The MDM pushes: the SCEP identity, the `kbf-mac-provision` settings profile (sleep off,
   `autorestart 1`, FileVault off, auto-login off, per mac-node-provisioning.md), and a
   signed, notarized `kbf-node.pkg`.
4. The pkg's postinstall does **not** enroll inline. The SCEP identity may not have arrived
   yet and a one-shot step would fail silently. It installs a `launchd` job with
   `KeepAlive` and backoff that waits for the identity, then runs `kbf-deploy node
   enroll`, fetches the spec slice, writes the daemon plist (flags, not environment) and
   loads it.

**Network and hosting for the MDM.** The device must reach the MDM URL, which needs a
certificate from a public CA (the OS trusts public roots only); the MDM host needs outbound
to Apple (ABM API, APNs) and DNS. The MDM URL does not have to be reachable from the
Internet if every Mac reaches it on the farm network [A: verify with one Mac]. The
MDM runs on a server host with `services = ["mdm"]`; if the farm network cannot reach
Apple, the MDM must sit on a host that can. This is a decision (CEO decision 4).

A Mac not bought through ABM is enrolled once by hand and is `trust = "test"` (4.5).

**Recovery without a console.** The Macs have no console. A wedged Mac is first
restarted or erased through the MDM (`EraseDevice` signed by an operator hardware key
through the gate, as designed). Only if the MDM cannot reach it does anyone need a display
and keyboard, so each Mac in a rack gets a cheap HDMI dummy plug, and the rack has one
KVM or a display on a cart.

### 5.3 What the operator does, and what happens

| Step | Operator | Automatic |
|---|---|---|
| Rack, cable, label | yes | none |
| Linux first power-on | `adopt` then power on | PXE, install, enroll, config, join, label; `ready` in about 15 minutes [A] |
| Mac first power-on | `mac assign`, add block, power on | ADE, SCEP, pkg, enroll, join |
| Replace a failed board | `replace` then `reinstall` (or power on) | same as new |

[A] marks an estimate nobody has measured on the reference hardware.

### 5.4 Server and storage nodes

The three server nodes follow 5.1 with `role = "server"`. `node apply` renders the
`kbf-server` unit, `kbf-ca`, a MinIO unit using `disks.minio` (distributed across the
three nodes), envoy, netboot and MDM as `services` lists them. Server nodes are never
reinstalled without an explicit `reinstall --server-confirm` (it prints the disks it will
not touch).

HA: this document assumes the three-voter metadata log in ha.md (open design). Until it
lands the inventory accepts one `voter = true` host; `check` fails if the voter count is
even or does not match what the server binary supports. Servers are never updated in
parallel.

### 5.5 Prerequisites and BIOS baseline

One page, done before the first host:

- **Network:** a provisioning VLAN with only the farm's hosts on it; DHCP (with reservations
  for the three servers) and proxyDHCP allowed; DNS names for `server_url` and `object_url`;
  switch DHCP snooping and port security; NTP source; outbound access, or an apt snapshot
  mirror, during install; for Macs outbound to Apple and a public-CA certificate for the MDM.
- **BIOS on every Linux host:** UEFI mode; PXE over IPv4 enabled and first in boot order
  (safe because netboot answers "boot local" when not armed); restore-on-AC-power
  set to on; BIOS admin password set; time set. **Secure Boot is off in the first cut**
  (iPXE is not shim-signed); a Secure-Boot-capable path through Ubuntu's signed shim and
  grub is a later option (CEO decision 3).
- **Management hardware to buy:** servers with a BMC that supports Redfish on a separate
  management network. kbf does not drive the BMC in the first cut (CEO decision 2).
- **Macs:** ethernet, FileVault off and `autorestart 1` per mac-node-provisioning.md (a
  Mac with FileVault on does not reach the network after a power cut without a person).

`kbf-deploy doctor` checks the parts it can reach (DNS, NTP, server reachable, netboot
reachable, certificate expiry of the Apple credentials) and is separate from `check`,
which is offline and deterministic.

## 6. Configuration

### 6.1 The inventory

```toml
# fleet.toml  (one file; this is the whole fleet)
[fleet]
name        = "example"
server_url  = "https://kbf.example.net:9443"   # where daemons dial
object_url  = "https://objects.example.net"    # signed specs and releases
netboot     = ["https://netboot.example.net:8443"]
roots       = ["ecdsa:...", "ecdsa:..."]        # current and standby (public halves)

[defaults.linux-builder]
role   = "daemon"
labels = { pool = "linux-x86" }
[defaults.mac-studio]
role   = "daemon"
labels = { pool = "mac-arm" }

[[host]]
name     = "build-001"
serial   = "S1234567"                  # from the purchase order and the chassis label
hwid     = "dmi:4c4c4544-0033-5a10-8052-b4c04f4a5a32"   # written by `adopt`
profile  = "linux-builder"
os_disk  = "/dev/disk/by-id/nvme-example-os"

[[host]]
name     = "build-002"
serial   = "S1234568"
profile  = "linux-builder"
os_disk  = "/dev/disk/by-id/nvme-example-os-2"
labels   = { gpu = "1" }               # per-host override
reserve_cpus = 4                       # daemon/all-in-one only

[[host]]
name     = "mac-001"
hwid     = "mac:F5KX1Y2Z3W"
profile  = "mac-studio"

[[host]]
name     = "srv-1"
serial   = "S7654321"
role     = "server"
services = ["kbf-server", "minio", "envoy", "netboot", "mdm"]
address  = "srv-1-reserved"           # name of a DHCP reservation; servers need a stable address
voter    = true                        # raft voter (section 5.4)
os_disk  = "/dev/disk/by-id/nvme-example-srv1-os"
disks    = { minio = ["/dev/disk/by-id/example-a", "/dev/disk/by-id/example-b"] }
```

`check` refuses a `minio` disk equal to `os_disk`, a missing `os_disk` on a Linux host,
a duplicate hwid, serial or name, an unknown profile, a placeholder, an even voter count.

The node name is what goes into the certificate (the one DNS name the server already
requires), the `--node-id` flag and every log line. It is chosen once and never derived.

- **Reinstall, disk swap:** same hwid, same name; nothing in the inventory changes.
- **Motherboard replacement:** the hwid changes. `kbf-deploy replace build-001 --serial <new>`
  revokes the old certificate by serial, clears the hwid and awaits `adopt` of the
  new board; name, labels and history follow the name.
- **Unknown hwid:** refused at every step; the inventory is an allowlist.

Labels the daemon detects (arch, CPUs, memory, ISA, GPU, drivers) are never typed. Only
operator intent is typed: `pool`, and any `--label k=v`.

### 6.2 One render, three roles

`kbf-deploy render <name>` produces the exact files a host runs.

| Role | Linux | macOS |
|---|---|---|
| `server` | `kbf-server.service`, `kbf-ca`, MinIO, envoy, netboot, MDM units as listed, nftables snippet, chrony | none |
| `daemon` | `kbf-daemon.service` (fresh rootless crun sandbox per action, daemon cache on), chrony | `com.kbf.daemon.plist` (native driver, daemon cache on) |
| `all-in-one` | `kbf-all-in-one.service` | the plist for the all-in-one binary |

Every setting is a flag. The renderer owns a table from inventory key to flag; a test
parses every rendered command line with the real binary's parser, so a rename fails the
build. `--reserve-cpus` is rendered only for `daemon` and `all-in-one`.

### 6.3 Hardware-derived defaults

- `--cpus`, memory, ISA, GPU, drivers: detected.
- Cache and scratch path: the first mounted non-root SSD (`lsblk` rotational=0), else
  `/var/lib/kbf`; written back to the node's status. A host can pin `cache_dir`.
- Disk layout for servers: only from the inventory, never guessed.

Override order, lowest to highest: built-in default, hardware detection,
`[defaults.<profile>]`, `[[host]]`. `render --explain` prints each value and its origin.

### 6.4 How a node gets its configuration

Pull, not push. `apply` (operator) checks the inventory, renders every host, and signs
**each host's slice** with the operator's delegated spec key. The slice is bound to the
node name, the role and the spec version, so a node cannot be fed another node's slice or a
role downgrade. It uploads under `fleet/spec/<version>/<name>/`. The node polls every few
minutes, downloading only its own path. **Authentication to the object store:** requests
go through envoy, which verifies the node certificate against the farm roots and the
revocation list and permits GET only on `fleet/spec/*/<own name>/` and the common
`fleet/releases/`. Nodes hold no store credentials. The node checks the signature,
version monotonicity, the delegated key's expiry and the name binding, and installs atomically
(write next to, fsync, rename). A bad spec is refused and reported; the previous
one keeps running.

Disclosure, accepted in writing: the release manifest is readable by every node
certificate; a host slice only by that host. A node cert holder learns the release hashes,
not other hosts' labels.

Pull is chosen over operator SSH because 29 hosts must converge without the operator's
laptop being online and the Macs have no SSH by policy.

## 7. Updates

Two things change: the **spec** (configuration) and the **software set** (binaries, OS
packages), both in the signed spec.

- Binaries: the spec names a release (build hashes), signed by the release key. The node
  downloads, verifies the signature and the hash, installs to a versioned directory,
  switches a symlink, restarts, and keeps the previous version.
- OS packages on Linux: the pinned apt snapshot, applied by `kbf-updater`. macOS updates
  stay through MDM.
- Rollout: `kbf-deploy rollout <release> --pool linux-x86 --canary 1 --batch 4
  --soak 30m`. For each node in a batch, using the server's cordon and drain (these
  exist): cordon, drain, set the desired release, wait for Hello with the new version,
  run a qualify action, uncordon. A failed gate stops the rollout and restores the previous release
  on that node. The record is written to the object store before the first node is
  touched, so a crashed operator resumes with `--resume`. The batch run is still
  driven from the operator's machine; the server-driven driver in fleet-updates.md
  replaces it later without changing the spec, and is pulled forward if the operator's
  machine being online for a 20-host rollout is a problem in practice.
- Servers: never in parallel; a voter updates only when the others are healthy; the
  leader last.
- Today the mechanism is scp the binary and restart by SHA.

A node that cannot reach the object store keeps running its last verified release (fail
static). A spec or release signature failure is an alert.

## 8. Cold start and disaster recovery

The netboot service, the CA and the object store live on the same three servers.

- **All servers down, disks intact:** power them on; services start from local state,
  MinIO re-forms with the drives; builders ran fail static and renew certificates
  within 15 days of the outage. Nothing to type.
- **Outage longer than 15 days:** certificates expired; after the servers return, run
  `kbf-deploy enroll-token --all` and paste the tokens, or reinstall (`reinstall --pool`).
- **All three servers lost:** (1) the offline root and its standby come from their
  safe place; (2) the inventory repo is the source of every host; (3) day-0 steps
  (section 14) rebuild the first server from USB, restore the registry backup from the
  operator's `backup pull`; (4) the CAS is a cache and is rebuilt by builds.
  What is lost: build cache contents, and revocations after the last backup (so
  `retire` is re-run for anything retired since).

## 9. The command set

`kbf-deploy` is a Linux and macOS binary (a Mac needs only `node` and `hwid`). It is one
operator CLI; the complete list of verbs is below, and a test asserts that `--help`
lists exactly these (so this table cannot drift).

| Group | Verbs |
|---|---|
| Inventory | `check` (offline lint), `doctor` (reachability, certificate and Apple-credential expiry), `render [--explain]`, `apply`, `status` (`--why`, `--audit`, `--buckconfig`) |
| Hosts | `add` (`--from-csv`, `--adopt-next`), `adopt`, `replace`, `retire`, `reinstall`, `enroll-token` (`<name>`, `--pool`, `--all`) |
| Updates | `rollout` (`--resume`, `--status`, `--batch`) |
| PKI | `ca init`, `ca sign`, `ca rotate`, `ca export-root`, `backup pull` |
| Provisioning | `netboot serve`, `netboot fetch`, `netboot usb`, `mac assign`, `mac unassign` |
| On the host | `node enroll`, `node apply`, `node renew`, `node doctor`, `hwid` |

That is about 30 invocations in six groups, not 11 verbs. The older kits fold in:
`minio`, `envoy` and `mdm` become `services` rendered by `node apply`; `linux
render/check/install/swap/image-pull` become `render`, `check`, `node apply` and `rollout`;
`pilot server-install` and `pilot buckconfig` become `render` output and `status
--buckconfig`. Today's verbs are kept as hidden aliases for one release, then removed.

Day-2 recipes:

| Task | Commands |
|---|---|
| Add a Linux host | `add build-021 --serial S...`; `apply`; rack; power on; `adopt build-021`; wait for `ready` |
| Add a Mac | `mac assign <serial>`; add its block; `apply`; power on |
| Remove a host | `retire build-007`; power off |
| Replace a failed board | `replace build-007 --serial <new>`; power on; `adopt build-007` |
| Replace a MinIO drive | edit `disks.minio`; `apply`; `node doctor` on the server prints the heal command and waits; never two drives on one node at once |
| Change a label or `reserve_cpus` | edit; `apply`; picked up within minutes, daemon restarts at an idle moment |
| Roll a release | `rollout <release> --pool ...` |
| Rebuild a host | `reinstall <name>` (arms one install; a live host does a one-shot network boot by itself, a dead one needs a power cycle) |
| Hosts that should have shown up | `status` lists any armed or inventory host not seen since expected, not only unknown ones |

## 10. What is retired

- `install.sh` and `install_units.sh`: replaced by `node apply` (first-boot unit, `.pkg`
  launchd job).
- The private repo's `deploy/` shell kits (`kbf-linux`, `kbf-pilot`): their logic is in
  `kbf-deploy` already or in section 5; the repo keeps only the inventory.
- `pilot_ca.sh` and the deny-list file: replaced by `ca`, `/enroll` and revocation.
- The Buildbarn-only scripts and one-time cutover scripts: deleted when the cutover ends.
- Dev-farm scripts (`kbf_dev_install.sh`, `upd_main.sh`, siblings): replaced by
  `rollout`; the dev farm becomes a second small inventory.
- Kit copies in scratch directories; hardcoded node numbers, subuid ranges and commit
  SHAs in scripts (the subuid range becomes an inventory default).

Each is removed in the change that makes its replacement work, never before.

## 11. Operator runbooks

### 11.1 Linux build host

1. Confirm the BIOS baseline (5.5). Note the vendor serial and put the **node name** label
   next to it.
2. `add` (or `add --from-csv`) and `apply`.
3. Rack, connect power and the provisioning-VLAN port, power on.
4. `status` shows `build-007: serial S seen ... adopt?`. Check the serial against the
   label and the address against the rack, then `adopt build-007`. A host you do not
   recognize stays unadopted; do not approve it.
5. Wait for `ready`. If not ready in 30 minutes: `status build-007 --why`. A 409 alert
   means another machine fetched the seed first: find it, re-arm with `reinstall`.
6. Never type a hostname or IP on a console.

### 11.2 Server and storage node

1. Drives in the bays named in `disks`; label each bay with the by-id suffix.
2. First node only: section 14.
3. Others: as 11.1 with `role = "server"`, a reserved address and `os_disk`.
4. `status srv-N` must show server healthy, MinIO drives online, voter joined. Do not power
   on a second server before the first shows it.
5. Drive failure: replace, update `disks.minio`, `apply`, `node doctor` on the host, wait
   for the heal.

### 11.3 Mac Studio

1. Serial in ABM, assigned to the farm's MDM; `mac assign <serial>`.
2. Rack, ethernet, HDMI dummy plug, power on. The first Mac needs a display and keyboard
   once.
3. Add the block, `apply`.
4. Wait for `ready`. A Mac stuck in Setup Assistant is not ADE-assigned: fix ABM, then
   erase through the MDM, or by display and keyboard from recovery as a last resort.
5. FileVault and auto-login follow mac-node-provisioning.md; do not change them by hand.

### 11.4 Outage longer than certificate renewal

See section 8. Run `enroll-token --all`, deliver tokens, check `status`.

## 12. What is still manual

- Racking, cabling, labeling; the BIOS baseline; the switch and VLAN setup.
- Day-0 root ceremony and first-server bootstrap (section 14).
- Matching each box to a name: `adopt` after checking the serial against the rack.
- ABM serial assignment; non-ABM Mac enrollment; the first Mac's display and keyboard.
- **Recurring, with the failure when it lapses:**
  - ABM DEP token, yearly: lapsed, new Macs do not enroll; existing Macs are unaffected.
  - APNs push certificate, yearly: lapsed, the MDM cannot reach any Mac. Renew under
    the same Apple account and certificate so enrollments survive.
  - Apple Developer ID (paid membership): lapsed, new notarized `.pkg` cannot be built;
    installed Macs are unaffected.
  - Intermediate CA and delegated spec keys, yearly: lapsed, no enrollments or spec
    publishes. `doctor` warns 60 and 30 days ahead for all of these.
- Drive replacement and heal; BMC-less power cycles.

## 13. Migration from today's hosts

Order, each step reversible:

0. Ship the durable registry (4.7). Nothing below enables `/enroll` before it.
1. Write `fleet.toml` for the existing hosts using `kbf-deploy hwid`; run `check`.
2. Ship `render` and diff its output against live units until empty; drift found is a
   finding.
3. Ship `/enroll`, `ca`, `node enroll`. Enroll existing hosts in place with tokens from
   `enroll-token`; the daemon switches at its next restart. The old shell CA stays valid
   until every host is enrolled, then is retired.
4. Ship the Linux node agent and `apply`; flip hosts one pool at a time, canary first.
5. Ship `rollout`.
6. Ship `netboot` and `adopt`; the next new Linux host is the first user. Existing hosts
   are never reinstalled for the sake of it.
7. Macs: **blocked on a macOS node agent, which does not exist yet.** Until it does, the
   Macs keep the current install. Then assign serials in ABM, hand-enroll those not
   bought through it, and replace the install with the pkg in place (same `_kbf` user and
   paths), verified on one Mac first.

Servers last; the single server is moved to `kbf-deploy` rendering before HA, so HA
arrives as an inventory change.

## 14. Day 0: building the farm from nothing

One linear list. Steps 1 to 6 need no object store.

1. Prerequisites (5.5) in place; Apple accounts and a public-CA certificate for the MDM
   obtained.
2. Root ceremony on an air-gapped machine: `ca init` makes root and standby; store both
   in separate safe places.
3. Write `fleet.toml` with the three servers; `apply --to-dir spec/` writes the signed
   bundle to a directory.
4. First server from a USB stick: `netboot usb srv-1 --spec-dir spec/`. First boot
   generates its intermediate request; carry it to the offline machine, `ca sign`,
   carry back.
5. `kbf-server`, `kbf-ca` and `netboot serve --spec-dir` run on srv-1; the registry is a
   file store.
6. `adopt srv-2` and `srv-3`; they install over PXE from srv-1, enroll and get their own
   intermediates (same offline signing).
7. MinIO forms across the three; envoy starts; `apply` now publishes the spec to the
   object store and `netboot` switches to `--spec-url`.
8. MDM on its host, ABM linked, DEP token and push certificate loaded.
9. Linux builders (11.1), then Macs (11.3).

## 15. Tests

- Renderer: golden units and plists per role; every rendered command line parsed by the
  real binary's parser; `--reserve-cpus` absent for servers.
- Inventory lint: one test per `check` refusal (placeholder, duplicate, even voters,
  `minio` disk equal to `os_disk`, missing `os_disk`).
- Enrollment with the fake CA: wrong hwid, wrong name, reused token (CAS), expired token,
  token in argv refused, live-identity conflict, revoked serial, retire then re-add the same
  hwid (old certificate stays dead), expired delegated key; each must fail.
- Netboot: an unarmed host gets "boot local"; an armed host gets the seed once and a 409 the
  second time; the seed never touches `disks.minio`; unknown report list capped.
- First-boot: a late-command that runs before the network is up retries, not fails.
- Rollout: crash the operator between canary and soak; `--resume` finishes without
  re-draining. Spec rollback below the stored version is refused.
- The CLI table in section 9 equals the real command tree.
- A planted mutant for each (a skipped signature check, a token consumed twice) must turn
  its test red.
- Hardware acceptance (section 4.2): iPXE uuid equals kernel `product_uuid`, per model.

## 16. Decisions for the CEO

1. **PXE plus autoinstall now, image-based later.** Recommended: yes.
2. **BMC scope.** Recommended: out of scope for the first cut; buy servers with a Redfish
   BMC on a management network; a later release adds one-shot PXE boot and power
   cycle through it. A live host reinstalls itself without one.
3. **Secure Boot.** Recommended: off in the first cut, BIOS password on, signed shim/grub
   path later.
4. **ABM and MDM hosting.** Recommended: accept the Apple dependency for Macs (ABM,
   DEP token, APNs, Developer ID, public-CA certificate); MDM on a server host if the
   farm network reaches Apple, else on a small separate host.
5. **Where the CA key lives.** Recommended: a separate `kbf-ca` process per server host
   (own user, name-constrained, TPM-sealed where present), accepting in writing that
   root on a server mints fleet-name certificates until its intermediate is revoked. A separate
   signer machine is the alternative; the interface is the same.
6. **Who holds spec signing.** Recommended: delegated key per operator on a hardware
   token, root-signed with expiry; CI signs only releases.
7. **TPM attestation.** Recommended: not in the first cut; the residual spoof risk is
   documented (4.2).
8. **Pull-based spec; 30-day certificates renewed at half-life.** Recommended: yes.
9. **Servers in the inventory before HA.** One voter first. Recommended: yes.

## 17. Open items

- Secure Enclave storage of the Mac node key (needs a TLS stack that signs through
  Security.framework).
- Switch-port or LLDP mapping of boxes to rack slots.
- TPM-backed enrollment.
- The macOS node agent; the CI build of iPXE with the root embedded.
- Unattended-enrollment details on the newest macOS (volume owner, bootstrap token),
  the MDM reachability assumption in 5.2, and the 15-minute install figure: all [A]
  until one Mac and one host are measured.
- Server HA behavior depends on ha.md.

## 18. Review record

Every must-fix from the two reviews was taken, with these exceptions and changes:

- **Per-node encryption of the spec** was not taken: the per-name path rule in envoy and
  the written disclosure (6.4) give the same property for less machinery.
- **A separate signer machine** was not taken; a separate process and user per server
  host is (decision 5), and a machine split stays possible.
- **BMC integration** was deferred, not refused: `reinstall` works without it for live
  hosts, and the purchase guidance asks for a BMC.
- **CI signing of specs** was replaced by per-operator delegated keys, which also fixes
  the bus factor and the audit gap without putting the spec key in CI.
- **TPM attestation and Secure Enclave keys** are deferred with the residual risk stated
  (4.2, 4.5).
- **"11 verbs"** was restated as a complete table with a test that keeps it true (9),
  and the claim is "one CLI, N components".
