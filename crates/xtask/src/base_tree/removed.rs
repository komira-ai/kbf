//! What the kbf base tree leaves out of the pinned base image: whole packages, and paths of
//! packages it keeps, each with the reason no action needs it. [`super::layers`] takes them out
//! of the tree as dpkg would.
//!
//! An action runs as one user in a network-less container whose root is the tree: it logs in to
//! nothing, changes no account, mounts nothing, installs no package and starts no service. What
//! the farm's actions use of the base (each file they open, run or look for on `PATH`) is the C
//! library and the libraries their tools load, `bash` and `dash`, coreutils, `sed`, `grep`, `awk`,
//! `diff`, `find`, `tar`, `gzip`, `hostname` and util-linux's `flock`; none of those is left out,
//! and no kept package depends on a package that is (the removal refuses that). The tree holds no
//! set-uid or set-gid file once these are out (`Rootfs::set_id_files` refuses one).

/// A package of the base image the kbf base tree leaves out, and why no action needs it.
pub(super) struct Removed {
    pub package: &'static str,
    pub why: &'static str,
}

/// A path of a package the kbf base tree keeps that it leaves out all the same, as dpkg's
/// `path-exclude` does (the package's file list still names it), and why no action needs it.
pub(super) struct Excluded {
    pub path: &'static str,
    pub why: &'static str,
}

/// The packages the kbf base tree leaves out of the pinned base, each with its reason.
pub(super) const REMOVED: &[Removed] = &[
    // Accounts, logins and authentication: the set-uid and set-gid programs of the base.
    Removed {
        package: "login",
        why: "login, newgrp (set-uid), sg, faillog, lastlog: logging in and switching groups",
    },
    Removed {
        package: "passwd",
        why: "passwd, chfn, chsh, gpasswd (set-uid), chage, expiry (set-gid), useradd and the other account tools",
    },
    Removed {
        package: "libpam-modules-bin",
        why: "unix_chkpwd and pam_extrausers_chkpwd (set-gid), the PAM password checkers",
    },
    Removed {
        package: "libpam-modules",
        why: "the PAM modules, used only by login, passwd and su",
    },
    Removed {
        package: "libpam-runtime",
        why: "the PAM configuration (/etc/pam.d) and pam-auth-update",
    },
    Removed {
        package: "base-passwd",
        why: "update-passwd and its master files: /etc/passwd and /etc/group stay as the image has them",
    },
    Removed {
        package: "libsemanage2",
        why: "SELinux policy management, used only by passwd",
    },
    Removed {
        package: "libsemanage-common",
        why: "the configuration of libsemanage2",
    },
    Removed {
        package: "libsepol2",
        why: "the SELinux policy library, used only by libsemanage2",
    },
    // Mounting and file systems.
    Removed {
        package: "mount",
        why: "mount, umount (set-uid), losetup, swapon: a job's mounts are made before it starts",
    },
    Removed {
        package: "e2fsprogs",
        why: "mkfs.ext4, e2fsck and the other ext2/3/4 tools",
    },
    Removed {
        package: "logsave",
        why: "a helper of e2fsck",
    },
    Removed {
        package: "libext2fs2t64",
        why: "the ext2/3/4 library of e2fsprogs",
    },
    Removed {
        package: "libss2",
        why: "the command-line library of e2fsprogs",
    },
    Removed {
        package: "libcom-err2",
        why: "the error-table library of e2fsprogs",
    },
    // Installing packages: no action installs one.
    Removed {
        package: "apt",
        why: "apt and apt-get: an action is network-less and installs nothing",
    },
    Removed {
        package: "libapt-pkg6.0t64",
        why: "the library of apt",
    },
    Removed {
        package: "gpgv",
        why: "the signature checker of apt",
    },
    Removed {
        package: "ubuntu-keyring",
        why: "the archive keys of apt",
    },
    Removed {
        package: "unminimize",
        why: "the script that reinstalls what the minimal image left out",
    },
    Removed {
        package: "libseccomp2",
        why: "used only by apt",
    },
    Removed {
        package: "libgnutls30t64",
        why: "TLS, used only by apt",
    },
    Removed {
        package: "libhogweed6t64",
        why: "used only by libgnutls30t64",
    },
    Removed {
        package: "libnettle8t64",
        why: "used only by libgnutls30t64 and libhogweed6t64",
    },
    Removed {
        package: "libp11-kit0",
        why: "used only by libgnutls30t64",
    },
    Removed {
        package: "libffi8",
        why: "used only by libp11-kit0",
    },
    Removed {
        package: "libtasn1-6",
        why: "used only by libgnutls30t64",
    },
    Removed {
        package: "libidn2-0",
        why: "used only by libgnutls30t64",
    },
    Removed {
        package: "libunistring5",
        why: "used only by libgnutls30t64 and libidn2-0",
    },
    Removed {
        package: "libassuan0",
        why: "used only by gpgv",
    },
    Removed {
        package: "libnpth0t64",
        why: "used only by gpgv",
    },
    Removed {
        package: "libxxhash0",
        why: "used only by libapt-pkg6.0t64",
    },
    Removed {
        package: "libdb5.3t64",
        why: "used only by libpam-modules",
    },
    Removed {
        package: "libdebconfclient0",
        why: "used only by base-passwd",
    },
    // Starting and stopping a system.
    Removed {
        package: "sysvinit-utils",
        why: "pidof, killall5 and the other helpers of init scripts: a job has no init",
    },
];

/// The paths of kept packages the kbf base tree leaves out, each with its reason.
pub(super) const EXCLUDED: &[Excluded] = &[Excluded {
    path: "/usr/bin/su",
    why: "su (set-uid) of util-linux, which stays for flock: an action switches to no other user",
}];
