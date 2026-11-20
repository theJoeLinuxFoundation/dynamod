//! A parsed tmpfiles.d line.

/// The line types (tmpfiles.d(5) "Type").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// `f` (create) and `f+` (create or truncate).
    CreateFile,
    /// `w`, `w+`.
    WriteFile,
    /// `d`.
    CreateDirectory,
    /// `D`: like `d`, contents removed by `--remove`.
    TruncateDirectory,
    /// `e`: adjust and clean existing directories.
    EmptyDirectory,
    /// `v`, `q`, `Q`: a plain directory here (no btrfs subvolumes are made).
    CreateSubvolume,
    /// `p`, `p+`.
    CreateFifo,
    /// `L`, `L+`, `L?`.
    CreateSymlink,
    /// `c`, `c+`.
    CreateCharDevice,
    /// `b`, `b+`.
    CreateBlockDevice,
    /// `C`, `C+`.
    CopyFiles,
    /// `x`.
    IgnorePath,
    /// `X`.
    IgnoreDirectoryPath,
    /// `r`.
    RemovePath,
    /// `R`.
    RecursiveRemovePath,
    /// `z` (and the old `m`).
    AdjustMode,
    /// `Z`.
    RecursiveAdjustMode,
    /// `t`.
    SetXattr,
    /// `T`.
    RecursiveSetXattr,
}

impl Kind {
    /// Lines that create or own their path come first among lines for the
    /// same path, so that the node exists before it is adjusted (systemd's
    /// `takes_ownership()`).
    pub fn takes_ownership(self) -> bool {
        !matches!(
            self,
            Kind::AdjustMode | Kind::RecursiveAdjustMode | Kind::SetXattr | Kind::RecursiveSetXattr
        )
    }

    /// Lines whose path is a shell glob (systemd's `needs_glob()`).
    pub fn takes_glob(self) -> bool {
        matches!(
            self,
            Kind::WriteFile
                | Kind::EmptyDirectory
                | Kind::IgnorePath
                | Kind::IgnoreDirectoryPath
                | Kind::RemovePath
                | Kind::RecursiveRemovePath
                | Kind::AdjustMode
                | Kind::RecursiveAdjustMode
                | Kind::SetXattr
                | Kind::RecursiveSetXattr
        )
    }

    /// Whether the line creates a directory (default mode 0755).
    pub fn is_directory(self) -> bool {
        matches!(
            self,
            Kind::CreateDirectory | Kind::TruncateDirectory | Kind::CreateSubvolume
        )
    }
}

/// Which timestamps an age is compared with (the "age-by" prefix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgeBy(pub u8);

impl AgeBy {
    pub const ATIME: u8 = 1;
    pub const BTIME: u8 = 2;
    pub const CTIME: u8 = 4;
    pub const MTIME: u8 = 8;
    /// Files: all four.
    pub const DEFAULT_FILE: AgeBy = AgeBy(Self::ATIME | Self::BTIME | Self::CTIME | Self::MTIME);
    /// Directories: not ctime, which cleaning itself changes.
    pub const DEFAULT_DIR: AgeBy = AgeBy(Self::ATIME | Self::BTIME | Self::MTIME);

    pub fn has(self, f: u8) -> bool {
        self.0 & f != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Age {
    /// Microseconds.
    pub usec: u64,
    /// `~`: leave the entries directly inside the directory, clean below.
    pub keep_first_level: bool,
    pub by_file: AgeBy,
    pub by_dir: AgeBy,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Origin {
    pub file: String,
    pub line: usize,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: Kind,
    /// The type letter (`m` read as `z`, `F` as `f`).
    pub letter: char,
    /// Absolute, simplified, inside the root (not prefixed with it).
    pub path: String,
    /// `+`.
    pub plus: bool,
    /// `!`.
    pub boot: bool,
    /// `-`: a failure to create does not count.
    pub allow_failure: bool,
    /// `=`: remove what is in the way if it has the wrong type.
    pub try_replace: bool,
    /// `?` on `L`: only if the target exists.
    pub only_if_target_exists: bool,
    /// `$`: removed by `--purge`.
    pub purge: bool,
    /// The mode to apply (or the default for new nodes when `!mode_set`).
    pub mode: u32,
    pub mode_set: bool,
    /// `~` before the mode.
    pub mode_mask: bool,
    /// `:` before the mode: only for new nodes.
    pub mode_only_create: bool,
    pub uid: Option<u32>,
    pub uid_only_create: bool,
    pub gid: Option<u32>,
    pub gid_only_create: bool,
    pub age: Option<Age>,
    /// After C unescaping, specifiers, Base64 (`~`) or a credential (`^`).
    pub argument: Option<Vec<u8>>,
    /// `c`, `b`.
    pub major_minor: Option<(u32, u32)>,
    /// `t`, `T`.
    pub xattrs: Vec<(String, Vec<u8>)>,
    pub origin: Origin,
}

impl Item {
    pub fn new(kind: Kind, letter: char, path: &str) -> Item {
        Item {
            kind,
            letter,
            path: path.to_string(),
            plus: false,
            boot: false,
            allow_failure: false,
            try_replace: false,
            only_if_target_exists: false,
            purge: false,
            mode: if kind.is_directory() { 0o755 } else { 0o644 },
            mode_set: false,
            mode_mask: false,
            mode_only_create: false,
            uid: None,
            uid_only_create: false,
            gid: None,
            gid_only_create: false,
            age: None,
            argument: None,
            major_minor: None,
            xattrs: Vec::new(),
            origin: Origin::default(),
        }
    }

    /// Two lines for the same path that both own it may coexist only if
    /// they ask for the same thing (systemd's `item_compatible()`).
    pub fn compatible(&self, other: &Item) -> bool {
        if !(self.kind.takes_ownership() && other.kind.takes_ownership()) {
            return true;
        }
        self.argument == other.argument
            && self.uid == other.uid
            && self.uid_only_create == other.uid_only_create
            && self.gid == other.gid
            && self.gid_only_create == other.gid_only_create
            && self.mode_set == other.mode_set
            && self.mode == other.mode
            && self.mode_only_create == other.mode_only_create
            && self.mode_mask == other.mode_mask
            && self.age == other.age
            && self.major_minor == other.major_minor
    }
}
