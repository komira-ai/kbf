//! The key a summary is learned under, and the day type time enters through.

/// The REAPI namespace a run belongs to.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instance(String);

impl Instance {
    /// An instance named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The instance's name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The OS and CPU class of the platform an action ran on (a test on arm64 and on x86_64
/// are sized apart).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlatformClass(String);

impl PlatformClass {
    /// A platform class named `name`.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The class's name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Which instrument produced the numbers. Footprint and cgroup `memory.peak` differ in
/// kind, so a sample of one never feeds the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Basis {
    /// Linux cgroup v2 counters.
    Cgroup,
    /// macOS peak physical footprint.
    MacFootprint,
}

/// What stays the same across builds of one target's action (the 8-byte shape key).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Shape(pub u64);

/// The configuration hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cfg(pub u64);

/// A day count (days since an epoch the caller chooses; only differences matter). The
/// crate never reads a clock: the caller passes the day in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Day(pub u32);

/// The level of the key hierarchy a summary lives at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// `(instance, platform, basis, shape, cfg)`.
    L1,
    /// `(instance, platform, basis, shape)`: the configuration changed or is unknown.
    L2,
}

/// The key of an action's summary. `cfg` is `None` for an action with no configuration
/// (such a key has only the L2 level).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    instance: Instance,
    platform: PlatformClass,
    basis: Basis,
    shape: Shape,
    cfg: Option<Cfg>,
}

impl Key {
    /// A key.
    pub fn new(
        instance: Instance,
        platform: PlatformClass,
        basis: Basis,
        shape: Shape,
        cfg: Option<Cfg>,
    ) -> Self {
        Self {
            instance,
            platform,
            basis,
            shape,
            cfg,
        }
    }

    /// The instance the key belongs to.
    pub fn instance(&self) -> &Instance {
        &self.instance
    }

    /// The platform class the key belongs to.
    pub fn platform(&self) -> &PlatformClass {
        &self.platform
    }

    /// The instrument this key's samples come from.
    pub fn basis(&self) -> Basis {
        self.basis
    }

    /// The action shape.
    pub fn shape(&self) -> Shape {
        self.shape
    }

    /// The configuration, `None` for a key with only the L2 level.
    pub fn cfg(&self) -> Option<Cfg> {
        self.cfg
    }

    /// The same key with the configuration dropped (level L2).
    pub fn l2(&self) -> Self {
        Self {
            cfg: None,
            ..self.clone()
        }
    }

    pub(crate) fn has_cfg(&self) -> bool {
        self.cfg.is_some()
    }
}
