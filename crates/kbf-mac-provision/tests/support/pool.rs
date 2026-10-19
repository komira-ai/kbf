//! The fake Mac's lease-user pool, its volume and a root daemon, as `apply` leaves them.

use super::{
    CONFIG, ERR_LOG, FakeMac, HOMES, KEY, PLIST, SCRATCH, VOLUME, golden_config, golden_plist_as,
    sha256_hex,
};

impl FakeMac {
    /// A converged Mac whose profile adds `daemon_user=root`, with every item as
    /// `apply` leaves it: the property list without `UserName`, the error log root's,
    /// the node key root's with mode 600, the scratch root root's with mode 755.
    pub fn converged_root(name: &str) -> Self {
        let mut mac = Self::converged(name);
        let text = format!("{}daemon_user=root\n", mac.profile);
        mac.set_profile(text);
        mac.own(ERR_LOG, 0, 0);
        mac.write(KEY, "key\n");
        mac.chmod(KEY, 0o600);
        mac.own(KEY, 0, 0);
        mac.mkdir(SCRATCH);
        mac.chmod(SCRATCH, 0o755);
        mac.own(SCRATCH, 0, 0);
        mac
    }

    /// The volume as `apply` leaves it: Spotlight off, its marker file, no backups.
    pub fn converge_volume(&mut self) {
        self.write(&format!("{VOLUME}/.metadata_never_index"), "");
        let s = self.state_mut();
        s.spotlight_off.insert(VOLUME.into());
        s.backup_excluded.insert(VOLUME.into());
    }

    /// Group `kbfpool` (gid `first`) and pool users `kbf-pool-0..n` with uids from
    /// `first`, as `apply` makes them, with their homes under [`HOMES`].
    pub fn converge_pool(&mut self, n: u32, first: u32) {
        self.mkdir(HOMES);
        self.chmod(HOMES, 0o755);
        self.own(HOMES, 0, 0);
        let gid = first.to_string();
        let s = self.state_mut();
        s.attr("/Groups/kbfpool", "PrimaryGroupID", &gid);
        s.attr("/Groups/kbfpool", "Password", "*");
        for i in 0..n {
            let name = format!("kbf-pool-{i}");
            let home = format!("{HOMES}/{name}");
            let uid = (first + i).to_string();
            let s = self.state_mut();
            for (a, v) in [
                ("UniqueID", uid.as_str()),
                ("PrimaryGroupID", gid.as_str()),
                ("UserShell", "/usr/bin/false"),
                ("NFSHomeDirectory", home.as_str()),
                ("IsHidden", "1"),
                ("Password", "*"),
            ] {
                s.user_attr(&name, a, v);
            }
            self.mkdir(&home);
            self.chmod(&home, 0o700);
            self.own(&home, first + i, first);
        }
    }

    /// Replaces the test profile of a converged Mac with `text`, and its property list
    /// with the golden one for `text` (without `UserName` when it sets
    /// `daemon_user=root`), and its `kbf.toml` with the golden one.
    pub fn set_profile(&mut self, text: String) {
        let user = (!text.contains("\ndaemon_user=root\n")).then_some("_kbf");
        self.write(
            PLIST,
            &golden_plist_as(&sha256_hex(text.as_bytes())[..12], user),
        );
        self.write(CONFIG, &golden_config(&sha256_hex(text.as_bytes())[..12]));
        self.profile = text;
    }
}
