//! The fake Mac's standing VM user `kbf-vm`, as `apply` leaves it.

use super::{FakeMac, LOGINWINDOW};

/// The VM user's uid in the tests' profile.
pub const VM_UID: u32 = 460;
pub const VM_PASSWORD_FILE: &str = "/var/db/kbf-mac-session/vm-user-password";
pub const VM_STATE_DIR: &str = "/var/db/kbf-mac-session";
/// The VM user's home, which is also the path of its directory record.
pub const VM_HOME: &str = concat!("/Users", "/kbf-vm");
pub const KCPASSWORD: &str = "/etc/kcpassword";
/// The password the converged fake's file holds: 64 hex digits.
pub const VM_PASSWORD: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

impl FakeMac {
    /// A converged Mac whose profile adds `vm_user_uid=460` (and, with `autologin`, sets
    /// `autologin=vm`), with the VM user as `apply` leaves it.
    pub fn converged_vm(name: &str, autologin: bool) -> Self {
        let mut mac = Self::converged(name);
        let mut text = format!("{}vm_user_uid={VM_UID}\n", mac.profile);
        if autologin {
            text = text.replace("\nautologin=off\n", "\nautologin=vm\n");
        }
        mac.set_profile(text);
        mac.converge_vm_user();
        if autologin {
            mac.converge_vm_autologin();
        }
        mac
    }

    /// The account, its home, the state directory and the password file.
    pub fn converge_vm_user(&mut self) {
        let uid = VM_UID.to_string();
        let s = self.state_mut();
        for (a, v) in [
            ("UniqueID", uid.as_str()),
            ("PrimaryGroupID", "20"),
            ("UserShell", "/bin/zsh"),
            ("NFSHomeDirectory", VM_HOME),
            ("IsHidden", "1"),
            ("Password", "********"),
            (
                "AuthenticationAuthority",
                ";ShadowHash;HASHLIST:<SALTED-SHA512-PBKDF2>",
            ),
        ] {
            s.user_attr("kbf-vm", a, v);
        }
        self.mkdir(VM_STATE_DIR);
        self.chmod(VM_STATE_DIR, 0o700);
        self.own(VM_STATE_DIR, 0, 0);
        self.write(VM_PASSWORD_FILE, VM_PASSWORD);
        self.chmod(VM_PASSWORD_FILE, 0o400);
        self.own(VM_PASSWORD_FILE, 0, 0);
        self.mkdir(VM_HOME);
        self.chmod(VM_HOME, 0o700);
        self.own(VM_HOME, VM_UID, 20);
    }

    /// Auto-login as the VM user, `/etc/kcpassword` holding the password of the file.
    pub fn converge_vm_autologin(&mut self) {
        self.state_mut()
            .default_set(LOGINWINDOW, "autoLoginUser", "kbf-vm");
        self.write_bytes(KCPASSWORD, &kcpassword_of(VM_PASSWORD));
    }

    /// Replaces the password file's text, keeping its mode 400 (the tests do not run
    /// as root, so a file they cannot write has to be opened up for the write).
    pub fn set_vm_password_file(&self, text: &str) {
        self.chmod(VM_PASSWORD_FILE, 0o600);
        self.write(VM_PASSWORD_FILE, text);
        self.chmod(VM_PASSWORD_FILE, 0o400);
    }

    /// Takes `autoLoginUser` out of the login window's preferences.
    pub fn clear_auto_login(&mut self) {
        self.state_mut()
            .defaults
            .remove(&(LOGINWINDOW.to_owned(), "autoLoginUser".to_owned()));
    }

    pub fn write_bytes(&self, path: &str, bytes: &[u8]) {
        std::fs::write(self.path(path), bytes).unwrap();
    }
}

/// `/etc/kcpassword` for `password`, worked out here independently of the crate: the
/// password and a NUL padded with zeros to a multiple of 12 bytes, XORed with the key.
pub fn kcpassword_of(password: &str) -> Vec<u8> {
    const KEY: [u8; 11] = [
        0x7D, 0x89, 0x52, 0x23, 0xD2, 0xBC, 0xDD, 0xEA, 0xA3, 0xB9, 0x1F,
    ];
    let mut bytes = password.as_bytes().to_vec();
    while !bytes.len().is_multiple_of(12) || bytes.len() == password.len() {
        bytes.push(0);
    }
    bytes
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ KEY[i % KEY.len()])
        .collect()
}
