#![allow(clippy::missing_safety_doc)]
#![allow(clippy::extra_unused_lifetimes)]

// This file is part of the uutils coreutils package.
//
// (c) Jordy Dickinson <jordy.dickinson@gmail.com>
// (c) Joshua S. Miller <jsmiller@uchicago.edu>
//
// For the full copyright and license information, please view the LICENSE file
// that was distributed with this source code.

// spell-checker:ignore (ToDO) copydir ficlone fiemap ftruncate linkgs lstat nlink nlinks pathbuf pwrite reflink strs xattrs symlinked deduplicated advcpmv

use quick_error::quick_error;
use std::borrow::Cow;
use std::collections::HashSet;
use std::env;
#[cfg(not(windows))]
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf, StripPrefixError};
use std::string::ToString;

use clap::{crate_version, Arg, ArgAction, ArgMatches, Command};
use filetime::FileTime;
use indicatif::{ProgressBar, ProgressStyle};
#[cfg(unix)]
use libc::mkfifo;
use quick_error::ResultExt;

use platform::copy_on_write;
use uucore::backup_control::{self, BackupMode};
use uucore::display::Quotable;
use uucore::error::{set_exit_code, UClapError, UError, UResult, UUsageError};
use uucore::fs::{
    canonicalize, paths_refer_to_same_file, FileInformation, MissingHandling, ResolveMode,
};
use uucore::{crash, format_usage, prompt_yes, show_error, show_warning};

use crate::copydir::copy_directory;

mod copydir;
mod platform;
quick_error! {
    #[derive(Debug)]
    pub enum Error {
        /// Simple io::Error wrapper
        IoErr(err: io::Error) { from() source(err) display("{}", err)}

        /// Wrapper for io::Error with path context
        IoErrContext(err: io::Error, path: String) {
            display("{}: {}", path, err)
            context(path: &'a str, err: io::Error) -> (err, path.to_owned())
            context(context: String, err: io::Error) -> (err, context)
            source(err)
        }

        /// General copy error
        Error(err: String) {
            display("{}", err)
            from(err: String) -> (err)
            from(err: &'static str) -> (err.to_string())
        }

        /// Represents the state when a non-fatal error has occurred
        /// and not all files were copied.
        NotAllFilesCopied {}

        /// Simple walkdir::Error wrapper
        WalkDirErr(err: walkdir::Error) { from() display("{}", err) source(err) }

        /// Simple std::path::StripPrefixError wrapper
        StripPrefixError(err: StripPrefixError) { from() }

        /// Result of a skipped file
        Skipped { }

        /// Result of a skipped file
        InvalidArgument(description: String) { display("{}", description) }

        /// All standard options are included as an an implementation
        /// path, but those that are not implemented yet should return
        /// a NotImplemented error.
        NotImplemented(opt: String) { display("Option '{}' not yet implemented.", opt) }

        /// Invalid arguments to backup
        Backup(description: String) { display("{}\nTry '{} --help' for more information.", description, uucore::execution_phrase()) }

        NotADirectory(path: String) { display("'{}' is not a directory", path) }
    }
}

impl UError for Error {
    fn code(&self) -> i32 {
        EXIT_ERR
    }
}

pub type CopyResult<T> = Result<T, Error>;
pub type Source = PathBuf;
pub type SourceSlice = Path;
pub type Target = PathBuf;
pub type TargetSlice = Path;

/// Specifies whether when overwrite files
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ClobberMode {
    Force,
    RemoveDestination,
    Standard,
}

/// Specifies whether when overwrite files
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum OverwriteMode {
    /// [Default] Always overwrite existing files
    Clobber(ClobberMode),
    /// Prompt before overwriting a file
    Interactive(ClobberMode),
    /// Never overwrite a file
    NoClobber,
}

/// Possible arguments for `--reflink`.
#[derive(Copy, Clone, Eq, PartialEq)]
pub enum ReflinkMode {
    Always,
    Auto,
    Never,
}

/// Possible arguments for `--sparse`.
#[derive(Copy, Clone, Eq, PartialEq)]
pub enum SparseMode {
    Always,
    Auto,
    Never,
}

/// Specifies the expected file type of copy target
pub enum TargetType {
    Directory,
    File,
}

pub enum CopyMode {
    Link,
    SymLink,
    Copy,
    Update,
    AttrOnly,
}

#[derive(Debug)]
pub struct Attributes {
    #[cfg(unix)]
    ownership: Preserve,
    mode: Preserve,
    timestamps: Preserve,
    context: Preserve,
    links: Preserve,
    xattr: Preserve,
}

impl Attributes {
    pub(crate) fn max(&mut self, other: Self) {
        #[cfg(unix)]
        {
            self.ownership = self.ownership.max(other.ownership);
        }
        self.mode = self.mode.max(other.mode);
        self.timestamps = self.timestamps.max(other.timestamps);
        self.context = self.context.max(other.context);
        self.links = self.links.max(other.links);
        self.xattr = self.xattr.max(other.xattr);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Preserve {
    No,
    Yes { required: bool },
}

impl Preserve {
    /// Preservation level should only increase, with no preservation being the lowest option,
    /// preserve but don't require - middle, and preserve and require - top.
    pub(crate) fn max(&self, other: Self) -> Self {
        match (self, other) {
            (Self::Yes { required: true }, _) | (_, Self::Yes { required: true }) => {
                Self::Yes { required: true }
            }
            (Self::Yes { required: false }, _) | (_, Self::Yes { required: false }) => {
                Self::Yes { required: false }
            }
            _ => Self::No,
        }
    }
}

/// Re-usable, extensible copy options
#[allow(dead_code)]
pub struct Options {
    attributes_only: bool,
    backup: BackupMode,
    copy_contents: bool,
    cli_dereference: bool,
    copy_mode: CopyMode,
    dereference: bool,
    no_target_dir: bool,
    one_file_system: bool,
    overwrite: OverwriteMode,
    parents: bool,
    sparse_mode: SparseMode,
    strip_trailing_slashes: bool,
    reflink_mode: ReflinkMode,
    attributes: Attributes,
    recursive: bool,
    backup_suffix: String,
    target_dir: Option<String>,
    update: bool,
    verbose: bool,
    progress_bar: bool,
}

static ABOUT: &str = "Copy SOURCE to DEST, or multiple SOURCE(s) to DIRECTORY.";
static EXIT_ERR: i32 = 1;

const USAGE: &str = "\
    {} [OPTION]... [-T] SOURCE DEST
    {} [OPTION]... SOURCE... DIRECTORY
    {} [OPTION]... -t DIRECTORY SOURCE...";

// Argument constants
mod options {
    pub const ARCHIVE: &str = "archive";
    pub const ATTRIBUTES_ONLY: &str = "attributes-only";
    pub const CLI_SYMBOLIC_LINKS: &str = "cli-symbolic-links";
    pub const CONTEXT: &str = "context";
    pub const COPY_CONTENTS: &str = "copy-contents";
    pub const DEREFERENCE: &str = "dereference";
    pub const FORCE: &str = "force";
    pub const INTERACTIVE: &str = "interactive";
    pub const LINK: &str = "link";
    pub const NO_CLOBBER: &str = "no-clobber";
    pub const NO_DEREFERENCE: &str = "no-dereference";
    pub const NO_DEREFERENCE_PRESERVE_LINKS: &str = "no-dereference-preserve-links";
    pub const NO_PRESERVE: &str = "no-preserve";
    pub const NO_TARGET_DIRECTORY: &str = "no-target-directory";
    pub const ONE_FILE_SYSTEM: &str = "one-file-system";
    pub const PARENT: &str = "parent";
    pub const PARENTS: &str = "parents";
    pub const PATHS: &str = "paths";
    pub const PROGRESS_BAR: &str = "progress";
    pub const PRESERVE: &str = "preserve";
    pub const PRESERVE_DEFAULT_ATTRIBUTES: &str = "preserve-default-attributes";
    pub const RECURSIVE: &str = "recursive";
    pub const REFLINK: &str = "reflink";
    pub const REMOVE_DESTINATION: &str = "remove-destination";
    pub const SPARSE: &str = "sparse";
    pub const STRIP_TRAILING_SLASHES: &str = "strip-trailing-slashes";
    pub const SYMBOLIC_LINK: &str = "symbolic-link";
    pub const TARGET_DIRECTORY: &str = "target-directory";
    pub const UPDATE: &str = "update";
    pub const VERBOSE: &str = "verbose";
}

#[cfg(unix)]
static PRESERVABLE_ATTRIBUTES: &[&str] = &[
    "mode",
    "ownership",
    "timestamps",
    "context",
    "links",
    "xattr",
    "all",
];

#[cfg(not(unix))]
static PRESERVABLE_ATTRIBUTES: &[&str] =
    &["mode", "timestamps", "context", "links", "xattr", "all"];

pub fn uu_app() -> Command {
    const MODE_ARGS: &[&str] = &[
        options::LINK,
        options::REFLINK,
        options::SYMBOLIC_LINK,
        options::ATTRIBUTES_ONLY,
        options::COPY_CONTENTS,
    ];
    Command::new(uucore::util_name())
        .version(crate_version!())
        .about(ABOUT)
        .override_usage(format_usage(USAGE))
        .infer_long_args(true)
        .arg(
            Arg::new(options::TARGET_DIRECTORY)
                .short('t')
                .conflicts_with(options::NO_TARGET_DIRECTORY)
                .long(options::TARGET_DIRECTORY)
                .value_name(options::TARGET_DIRECTORY)
                .value_hint(clap::ValueHint::DirPath)
                .help("copy all SOURCE arguments into target-directory"),
        )
        .arg(
            Arg::new(options::NO_TARGET_DIRECTORY)
                .short('T')
                .long(options::NO_TARGET_DIRECTORY)
                .conflicts_with(options::TARGET_DIRECTORY)
                .help("Treat DEST as a regular file and not a directory")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::INTERACTIVE)
                .short('i')
                .long(options::INTERACTIVE)
                .overrides_with(options::NO_CLOBBER)
                .help("ask before overwriting files")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::LINK)
                .short('l')
                .long(options::LINK)
                .overrides_with_all(MODE_ARGS)
                .help("hard-link files instead of copying")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::NO_CLOBBER)
                .short('n')
                .long(options::NO_CLOBBER)
                .overrides_with(options::INTERACTIVE)
                .help("don't overwrite a file that already exists")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::RECURSIVE)
                .short('r')
                .visible_short_alias('R')
                .long(options::RECURSIVE)
                // --archive sets this option
                .help("copy directories recursively")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::STRIP_TRAILING_SLASHES)
                .long(options::STRIP_TRAILING_SLASHES)
                .help("remove any trailing slashes from each SOURCE argument")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::VERBOSE)
                .short('v')
                .long(options::VERBOSE)
                .help("explicitly state what is being done")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::SYMBOLIC_LINK)
                .short('s')
                .long(options::SYMBOLIC_LINK)
                .overrides_with_all(MODE_ARGS)
                .help("make symbolic links instead of copying")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::FORCE)
                .short('f')
                .long(options::FORCE)
                .help(
                    "if an existing destination file cannot be opened, remove it and \
                    try again (this option is ignored when the -n option is also used). \
                    Currently not implemented for Windows.",
                )
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::REMOVE_DESTINATION)
                .long(options::REMOVE_DESTINATION)
                .overrides_with(options::FORCE)
                .help(
                    "remove each existing destination file before attempting to open it \
                    (contrast with --force). On Windows, currently only works for \
                    writeable files.",
                )
                .action(ArgAction::SetTrue),
        )
        .arg(backup_control::arguments::backup())
        .arg(backup_control::arguments::backup_no_args())
        .arg(backup_control::arguments::suffix())
        .arg(
            Arg::new(options::UPDATE)
                .short('u')
                .long(options::UPDATE)
                .help(
                    "copy only when the SOURCE file is newer than the destination file \
                    or when the destination file is missing",
                )
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::REFLINK)
                .long(options::REFLINK)
                .value_name("WHEN")
                .overrides_with_all(MODE_ARGS)
                .require_equals(true)
                .default_missing_value("always")
                .value_parser(["auto", "always", "never"])
                .num_args(0..=1)
                .help("control clone/CoW copies. See below"),
        )
        .arg(
            Arg::new(options::ATTRIBUTES_ONLY)
                .long(options::ATTRIBUTES_ONLY)
                .overrides_with_all(MODE_ARGS)
                .help("Don't copy the file data, just the attributes")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::PRESERVE)
                .long(options::PRESERVE)
                .action(ArgAction::Append)
                .use_value_delimiter(true)
                .value_parser(clap::builder::PossibleValuesParser::new(
                    PRESERVABLE_ATTRIBUTES,
                ))
                .num_args(0..)
                .value_name("ATTR_LIST")
                .overrides_with_all([
                    options::ARCHIVE,
                    options::PRESERVE_DEFAULT_ATTRIBUTES,
                    options::NO_PRESERVE,
                ])
                // -d sets this option
                // --archive sets this option
                .help(
                    "Preserve the specified attributes (default: mode, ownership (unix only), \
                     timestamps), if possible additional attributes: context, links, xattr, all",
                ),
        )
        .arg(
            Arg::new(options::PRESERVE_DEFAULT_ATTRIBUTES)
                .short('p')
                .long(options::PRESERVE_DEFAULT_ATTRIBUTES)
                .overrides_with_all([options::PRESERVE, options::NO_PRESERVE, options::ARCHIVE])
                .help("same as --preserve=mode,ownership(unix only),timestamps")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::NO_PRESERVE)
                .long(options::NO_PRESERVE)
                .value_name("ATTR_LIST")
                .overrides_with_all([
                    options::PRESERVE_DEFAULT_ATTRIBUTES,
                    options::PRESERVE,
                    options::ARCHIVE,
                ])
                .help("don't preserve the specified attributes"),
        )
        .arg(
            Arg::new(options::PARENTS)
                .long(options::PARENTS)
                .alias(options::PARENT)
                .help("use full source file name under DIRECTORY")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::NO_DEREFERENCE)
                .short('P')
                .long(options::NO_DEREFERENCE)
                .overrides_with(options::DEREFERENCE)
                // -d sets this option
                .help("never follow symbolic links in SOURCE")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::DEREFERENCE)
                .short('L')
                .long(options::DEREFERENCE)
                .overrides_with(options::NO_DEREFERENCE)
                .help("always follow symbolic links in SOURCE")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::CLI_SYMBOLIC_LINKS)
                .short('H')
                .help("follow command-line symbolic links in SOURCE")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::ARCHIVE)
                .short('a')
                .long(options::ARCHIVE)
                .overrides_with_all([
                    options::PRESERVE_DEFAULT_ATTRIBUTES,
                    options::PRESERVE,
                    options::NO_PRESERVE,
                ])
                .help("Same as -dR --preserve=all")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::NO_DEREFERENCE_PRESERVE_LINKS)
                .short('d')
                .help("same as --no-dereference --preserve=links")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::ONE_FILE_SYSTEM)
                .short('x')
                .long(options::ONE_FILE_SYSTEM)
                .help("stay on this file system")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::SPARSE)
                .long(options::SPARSE)
                .value_name("WHEN")
                .value_parser(["never", "auto", "always"])
                .help("NotImplemented: control creation of sparse files. See below"),
        )
        // TODO: implement the following args
        .arg(
            Arg::new(options::COPY_CONTENTS)
                .long(options::COPY_CONTENTS)
                .overrides_with(options::ATTRIBUTES_ONLY)
                .help("NotImplemented: copy contents of special files when recursive")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new(options::CONTEXT)
                .long(options::CONTEXT)
                .value_name("CTX")
                .help(
                    "NotImplemented: set SELinux security context of destination file to \
                    default type",
                ),
        )
        // END TODO
        .arg(
            // The 'g' short flag is modeled after advcpmv
            // See this repo: https://github.com/jarun/advcpmv
            Arg::new(options::PROGRESS_BAR)
                .long(options::PROGRESS_BAR)
                .short('g')
                .action(clap::ArgAction::SetTrue)
                .help(
                    "Display a progress bar. \n\
                Note: this feature is not supported by GNU coreutils.",
                ),
        )
        .arg(
            Arg::new(options::PATHS)
                .action(ArgAction::Append)
                .value_hint(clap::ValueHint::AnyPath),
        )
}

#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let matches = uu_app()
        .after_help(backup_control::BACKUP_CONTROL_LONG_HELP)
        .try_get_matches_from(args);

    // The error is parsed here because we do not want version or help being printed to stderr.
    if let Err(e) = matches {
        let mut app = uu_app().after_help(backup_control::BACKUP_CONTROL_LONG_HELP);

        match e.kind() {
            clap::error::ErrorKind::DisplayHelp => {
                app.print_help()?;
            }
            clap::error::ErrorKind::DisplayVersion => print!("{}", app.render_version()),
            _ => return Err(Box::new(e.with_exit_code(1))),
        };
    } else if let Ok(matches) = matches {
        let options = Options::from_matches(&matches)?;

        if options.overwrite == OverwriteMode::NoClobber && options.backup != BackupMode::NoBackup {
            return Err(UUsageError::new(
                EXIT_ERR,
                "options --backup and --no-clobber are mutually exclusive",
            ));
        }

        let paths: Vec<String> = matches
            .get_many::<String>(options::PATHS)
            .map(|v| v.map(ToString::to_string).collect())
            .unwrap_or_default();

        let (sources, target) = parse_path_args(&paths, &options)?;

        if let Err(error) = copy(&sources, &target, &options) {
            match error {
                // Error::NotAllFilesCopied is non-fatal, but the error
                // code should still be EXIT_ERR as does GNU cp
                Error::NotAllFilesCopied => {}
                // Else we caught a fatal bubbled-up error, log it to stderr
                _ => show_error!("{}", error),
            };
            set_exit_code(EXIT_ERR);
        }
    }

    Ok(())
}

impl ClobberMode {
    fn from_matches(matches: &ArgMatches) -> Self {
        if matches.get_flag(options::FORCE) {
            Self::Force
        } else if matches.get_flag(options::REMOVE_DESTINATION) {
            Self::RemoveDestination
        } else {
            Self::Standard
        }
    }
}

impl OverwriteMode {
    fn from_matches(matches: &ArgMatches) -> Self {
        if matches.get_flag(options::INTERACTIVE) {
            Self::Interactive(ClobberMode::from_matches(matches))
        } else if matches.get_flag(options::NO_CLOBBER) {
            Self::NoClobber
        } else {
            Self::Clobber(ClobberMode::from_matches(matches))
        }
    }
}

impl CopyMode {
    fn from_matches(matches: &ArgMatches) -> Self {
        if matches.get_flag(options::LINK) {
            Self::Link
        } else if matches.get_flag(options::SYMBOLIC_LINK) {
            Self::SymLink
        } else if matches.get_flag(options::UPDATE) {
            Self::Update
        } else if matches.get_flag(options::ATTRIBUTES_ONLY) {
            Self::AttrOnly
        } else {
            Self::Copy
        }
    }
}

impl Attributes {
    // TODO: ownership is required if the user is root, for non-root users it's not required.
    // See: https://github.com/coreutils/coreutils/blob/master/src/copy.c#L3181

    fn all() -> Self {
        Self {
            #[cfg(unix)]
            ownership: Preserve::Yes { required: true },
            mode: Preserve::Yes { required: true },
            timestamps: Preserve::Yes { required: true },
            context: {
                #[cfg(feature = "feat_selinux")]
                {
                    Preserve::Yes { required: false }
                }
                #[cfg(not(feature = "feat_selinux"))]
                {
                    Preserve::No
                }
            },
            links: Preserve::Yes { required: true },
            xattr: Preserve::Yes { required: false },
        }
    }

    fn default() -> Self {
        Self {
            #[cfg(unix)]
            ownership: Preserve::Yes { required: true },
            mode: Preserve::Yes { required: true },
            timestamps: Preserve::Yes { required: true },
            context: Preserve::No,
            links: Preserve::No,
            xattr: Preserve::No,
        }
    }

    fn none() -> Self {
        Self {
            #[cfg(unix)]
            ownership: Preserve::No,
            mode: Preserve::No,
            timestamps: Preserve::No,
            context: Preserve::No,
            links: Preserve::No,
            xattr: Preserve::No,
        }
    }

    /// Tries to match string containing a parameter to preserve with the corresponding entry in the
    /// Attributes struct.
    fn try_set_from_string(&mut self, value: &str) -> Result<(), Error> {
        let preserve_yes_required = Preserve::Yes { required: true };

        match &*value.to_lowercase() {
            "mode" => self.mode = preserve_yes_required,
            #[cfg(unix)]
            "ownership" => self.ownership = preserve_yes_required,
            "timestamps" => self.timestamps = preserve_yes_required,
            "context" => self.context = preserve_yes_required,
            "links" => self.links = preserve_yes_required,
            "xattr" => self.xattr = preserve_yes_required,
            _ => {
                return Err(Error::InvalidArgument(format!(
                    "invalid attribute {}",
                    value.quote()
                )));
            }
        };
        Ok(())
    }
}

impl Options {
    fn from_matches(matches: &ArgMatches) -> CopyResult<Self> {
        let not_implemented_opts = vec![
            #[cfg(not(any(windows, unix)))]
            options::ONE_FILE_SYSTEM,
            options::CONTEXT,
            #[cfg(windows)]
            options::FORCE,
        ];

        for not_implemented_opt in not_implemented_opts {
            if matches.contains_id(not_implemented_opt)
                && matches.value_source(not_implemented_opt)
                    == Some(clap::parser::ValueSource::CommandLine)
            {
                return Err(Error::NotImplemented(not_implemented_opt.to_string()));
            }
        }

        let recursive = matches.get_flag(options::RECURSIVE) || matches.get_flag(options::ARCHIVE);

        let backup_mode = match backup_control::determine_backup_mode(matches) {
            Err(e) => return Err(Error::Backup(format!("{e}"))),
            Ok(mode) => mode,
        };

        let backup_suffix = backup_control::determine_backup_suffix(matches);

        let overwrite = OverwriteMode::from_matches(matches);

        // Parse target directory options
        let no_target_dir = matches.get_flag(options::NO_TARGET_DIRECTORY);
        let target_dir = matches
            .get_one::<String>(options::TARGET_DIRECTORY)
            .map(ToString::to_string);

        if let Some(dir) = &target_dir {
            if !Path::new(dir).is_dir() {
                return Err(Error::NotADirectory(dir.clone()));
            }
        };

        // Parse attributes to preserve
        let attributes: Attributes = if matches.contains_id(options::PRESERVE) {
            match matches.get_many::<String>(options::PRESERVE) {
                None => Attributes::default(),
                Some(attribute_strs) => {
                    let mut attributes: Attributes = Attributes::none();
                    let mut attributes_empty = true;
                    for attribute_str in attribute_strs {
                        attributes_empty = false;
                        if attribute_str == "all" {
                            attributes.max(Attributes::all());
                        } else {
                            attributes.try_set_from_string(attribute_str)?;
                        }
                    }
                    // `--preserve` case, use the defaults
                    if attributes_empty {
                        Attributes::default()
                    } else {
                        attributes
                    }
                }
            }
        } else if matches.get_flag(options::ARCHIVE) {
            // --archive is used. Same as --preserve=all
            Attributes::all()
        } else if matches.get_flag(options::NO_DEREFERENCE_PRESERVE_LINKS) {
            let mut attributes = Attributes::none();
            attributes.links = Preserve::Yes { required: true };
            attributes
        } else if matches.get_flag(options::PRESERVE_DEFAULT_ATTRIBUTES) {
            Attributes::default()
        } else {
            Attributes::none()
        };

        #[cfg(not(feature = "feat_selinux"))]
        if let Preserve::Yes { required } = attributes.context {
            let selinux_disabled_error =
                Error::Error("SELinux was not enabled during the compile time!".to_string());
            if required {
                return Err(selinux_disabled_error);
            } else {
                show_error_if_needed(&selinux_disabled_error);
            }
        }

        let options = Self {
            attributes_only: matches.get_flag(options::ATTRIBUTES_ONLY),
            copy_contents: matches.get_flag(options::COPY_CONTENTS),
            cli_dereference: matches.get_flag(options::CLI_SYMBOLIC_LINKS),
            copy_mode: CopyMode::from_matches(matches),
            // No dereference is set with -p, -d and --archive
            dereference: !(matches.get_flag(options::NO_DEREFERENCE)
                || matches.get_flag(options::NO_DEREFERENCE_PRESERVE_LINKS)
                || matches.get_flag(options::ARCHIVE)
                || recursive)
                || matches.get_flag(options::DEREFERENCE),
            one_file_system: matches.get_flag(options::ONE_FILE_SYSTEM),
            parents: matches.get_flag(options::PARENTS),
            update: matches.get_flag(options::UPDATE),
            verbose: matches.get_flag(options::VERBOSE),
            strip_trailing_slashes: matches.get_flag(options::STRIP_TRAILING_SLASHES),
            reflink_mode: {
                if let Some(reflink) = matches.get_one::<String>(options::REFLINK) {
                    match reflink.as_str() {
                        "always" => ReflinkMode::Always,
                        "auto" => ReflinkMode::Auto,
                        "never" => ReflinkMode::Never,
                        value => {
                            return Err(Error::InvalidArgument(format!(
                                "invalid argument {} for \'reflink\'",
                                value.quote()
                            )));
                        }
                    }
                } else {
                    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
                    {
                        ReflinkMode::Auto
                    }
                    #[cfg(not(any(
                        target_os = "linux",
                        target_os = "android",
                        target_os = "macos"
                    )))]
                    {
                        ReflinkMode::Never
                    }
                }
            },
            sparse_mode: {
                if let Some(val) = matches.get_one::<String>(options::SPARSE) {
                    match val.as_str() {
                        "always" => SparseMode::Always,
                        "auto" => SparseMode::Auto,
                        "never" => SparseMode::Never,
                        _ => {
                            return Err(Error::InvalidArgument(format!(
                                "invalid argument {val} for \'sparse\'"
                            )));
                        }
                    }
                } else {
                    SparseMode::Auto
                }
            },
            backup: backup_mode,
            backup_suffix,
            overwrite,
            no_target_dir,
            attributes,
            recursive,
            target_dir,
            progress_bar: matches.get_flag(options::PROGRESS_BAR),
        };

        Ok(options)
    }

    fn dereference(&self, in_command_line: bool) -> bool {
        self.dereference || (in_command_line && self.cli_dereference)
    }

    fn preserve_hard_links(&self) -> bool {
        match self.attributes.links {
            Preserve::No => false,
            Preserve::Yes { .. } => true,
        }
    }

    /// Whether to force overwriting the destination file.
    fn force(&self) -> bool {
        matches!(self.overwrite, OverwriteMode::Clobber(ClobberMode::Force))
    }
}

impl TargetType {
    /// Return TargetType required for `target`.
    ///
    /// Treat target as a dir if we have multiple sources or the target
    /// exists and already is a directory
    fn determine(sources: &[Source], target: &TargetSlice) -> Self {
        if sources.len() > 1 || target.is_dir() {
            Self::Directory
        } else {
            Self::File
        }
    }
}

/// Returns tuple of (Source paths, Target)
fn parse_path_args(path_args: &[String], options: &Options) -> CopyResult<(Vec<Source>, Target)> {
    let mut paths = path_args.iter().map(PathBuf::from).collect::<Vec<_>>();

    if paths.is_empty() {
        // No files specified
        return Err("missing file operand".into());
    }

    // Return an error if the user requested to copy more than one
    // file source to a file target
    if options.no_target_dir && options.target_dir.is_none() && paths.len() > 2 {
        return Err(format!("extra operand {:?}", paths[2]).into());
    }

    let target = match options.target_dir {
        Some(ref target) => {
            // All path args are sources, and the target dir was
            // specified separately
            PathBuf::from(target)
        }
        None => {
            // If there was no explicit target-dir, then use the last
            // path_arg
            paths.pop().unwrap()
        }
    };

    if options.strip_trailing_slashes {
        for source in &mut paths {
            *source = source.components().as_path().to_owned();
        }
    }

    Ok((paths, target))
}

/// Get the inode information for a file.
fn get_inode(file_info: &FileInformation) -> u64 {
    #[cfg(unix)]
    let result = file_info.inode();
    #[cfg(windows)]
    let result = file_info.file_index();
    result
}

#[cfg(target_os = "redox")]
fn preserve_hardlinks(
    hard_links: &mut Vec<(String, u64)>,
    source: &std::path::Path,
    dest: &std::path::Path,
    found_hard_link: &mut bool,
) -> CopyResult<()> {
    // Redox does not currently support hard links
    Ok(())
}

/// Hard link a pair of files if needed _and_ record if this pair is a new hard link.
#[cfg(not(target_os = "redox"))]
fn preserve_hardlinks(
    hard_links: &mut Vec<(String, u64)>,
    source: &std::path::Path,
    dest: &std::path::Path,
) -> CopyResult<bool> {
    let info = FileInformation::from_path(source, false)
        .context(format!("cannot stat {}", source.quote()))?;
    let inode = get_inode(&info);
    let nlinks = info.number_of_links();
    let mut found_hard_link = false;
    for (link, link_inode) in hard_links.iter() {
        if *link_inode == inode {
            // Consider the following files:
            //
            // * `src/f` - a regular file
            // * `src/link` - a hard link to `src/f`
            // * `dest/src/f` - a different regular file
            //
            // In this scenario, if we do `cp -a src/ dest/`, it is
            // possible that the order of traversal causes `src/link`
            // to get copied first (to `dest/src/link`). In that case,
            // in order to make sure `dest/src/link` is a hard link to
            // `dest/src/f` and `dest/src/f` has the contents of
            // `src/f`, we delete the existing file to allow the hard
            // linking.
            if file_or_link_exists(dest) && file_or_link_exists(Path::new(link)) {
                std::fs::remove_file(dest)?;
            }
            std::fs::hard_link(link, dest).unwrap();
            found_hard_link = true;
        }
    }
    if !found_hard_link && nlinks > 1 {
        hard_links.push((dest.to_str().unwrap().to_string(), inode));
    }
    Ok(found_hard_link)
}

/// When handling errors, we don't always want to show them to the user. This function handles that.
/// If the error is printed, returns true, false otherwise.
fn show_error_if_needed(error: &Error) -> bool {
    match error {
        // When using --no-clobber, we don't want to show
        // an error message
        Error::NotAllFilesCopied => (),
        Error::Skipped => (),
        _ => {
            show_error!("{}", error);
            return true;
        }
    }
    false
}

/// Copy all `sources` to `target`.  Returns an
/// `Err(Error::NotAllFilesCopied)` if at least one non-fatal error was
/// encountered.
///
/// Behavior depends on path`options`, see [`Options`] for details.
///
/// [`Options`]: ./struct.Options.html
fn copy(sources: &[Source], target: &TargetSlice, options: &Options) -> CopyResult<()> {
    let target_type = TargetType::determine(sources, target);
    verify_target_type(target, &target_type)?;

    let preserve_hard_links = options.preserve_hard_links();

    let mut hard_links: Vec<(String, u64)> = vec![];

    let mut non_fatal_errors = false;
    let mut seen_sources = HashSet::with_capacity(sources.len());
    let mut symlinked_files = HashSet::new();

    let progress_bar = if options.progress_bar {
        let pb = ProgressBar::new(disk_usage(sources, options.recursive)?)
            .with_style(
                ProgressStyle::with_template(
                    "{msg}: [{elapsed_precise}] {wide_bar} {bytes:>7}/{total_bytes:7}",
                )
                .unwrap(),
            )
            .with_message(uucore::util_name());
        pb.tick();
        Some(pb)
    } else {
        None
    };

    for source in sources.iter() {
        if seen_sources.contains(source) {
            // FIXME: compare sources by the actual file they point to, not their path. (e.g. dir/file == dir/../dir/file in most cases)
            show_warning!("source {} specified more than once", source.quote());
        } else {
            let found_hard_link = if preserve_hard_links && !source.is_dir() {
                let dest = construct_dest_path(source, target, &target_type, options)?;
                preserve_hardlinks(&mut hard_links, source, &dest)?
            } else {
                false
            };
            if !found_hard_link {
                if let Err(error) = copy_source(
                    &progress_bar,
                    source,
                    target,
                    &target_type,
                    options,
                    &mut symlinked_files,
                ) {
                    if show_error_if_needed(&error) {
                        non_fatal_errors = true;
                    }
                }
            }
            seen_sources.insert(source);
        }
    }
    if non_fatal_errors {
        Err(Error::NotAllFilesCopied)
    } else {
        Ok(())
    }
}

fn construct_dest_path(
    source_path: &Path,
    target: &TargetSlice,
    target_type: &TargetType,
    options: &Options,
) -> CopyResult<PathBuf> {
    if options.no_target_dir && target.is_dir() {
        return Err(format!(
            "cannot overwrite directory {} with non-directory",
            target.quote()
        )
        .into());
    }

    if options.parents && !target.is_dir() {
        return Err("with --parents, the destination must be a directory".into());
    }

    Ok(match *target_type {
        TargetType::Directory => {
            let root = if options.parents {
                Path::new("")
            } else {
                source_path.parent().unwrap_or(source_path)
            };
            localize_to_target(root, source_path, target)?
        }
        TargetType::File => target.to_path_buf(),
    })
}

fn copy_source(
    progress_bar: &Option<ProgressBar>,
    source: &SourceSlice,
    target: &TargetSlice,
    target_type: &TargetType,
    options: &Options,
    symlinked_files: &mut HashSet<FileInformation>,
) -> CopyResult<()> {
    let source_path = Path::new(&source);
    if source_path.is_dir() {
        // Copy as directory
        copy_directory(progress_bar, source, target, options, symlinked_files, true)
    } else {
        // Copy as file
        let dest = construct_dest_path(source_path, target, target_type, options)?;
        copy_file(
            progress_bar,
            source_path,
            dest.as_path(),
            options,
            symlinked_files,
            true,
        )
    }
}

impl OverwriteMode {
    fn verify(&self, path: &Path) -> CopyResult<()> {
        match *self {
            Self::NoClobber => Err(Error::NotAllFilesCopied),
            Self::Interactive(_) => {
                if prompt_yes!("overwrite {}?", path.quote()) {
                    Ok(())
                } else {
                    Err(Error::Skipped)
                }
            }
            Self::Clobber(_) => Ok(()),
        }
    }
}

/// Handles errors for attributes preservation. If the attribute is not required, and
/// errored, tries to show error (see `show_error_if_needed` for additional behavior details).
/// If it's required, then the error is thrown.
fn handle_preserve<F: Fn() -> CopyResult<()>>(p: &Preserve, f: F) -> CopyResult<()> {
    match p {
        Preserve::No => {}
        Preserve::Yes { required } => {
            let result = f();
            if *required {
                result?;
            } else if let Err(error) = result {
                show_error_if_needed(&error);
            }
        }
    };
    Ok(())
}

/// Copy the specified attributes from one path to another.
pub(crate) fn copy_attributes(
    source: &Path,
    dest: &Path,
    attributes: &Attributes,
) -> CopyResult<()> {
    let context = &*format!("{} -> {}", source.quote(), dest.quote());
    let source_metadata = fs::symlink_metadata(source).context(context)?;

    // Ownership must be changed first to avoid interfering with mode change.
    #[cfg(unix)]
    handle_preserve(&attributes.ownership, || -> CopyResult<()> {
        use std::os::unix::prelude::MetadataExt;
        use uucore::perms::wrap_chown;
        use uucore::perms::Verbosity;
        use uucore::perms::VerbosityLevel;

        let dest_uid = source_metadata.uid();
        let dest_gid = source_metadata.gid();

        wrap_chown(
            dest,
            &dest.symlink_metadata().context(context)?,
            Some(dest_uid),
            Some(dest_gid),
            false,
            Verbosity {
                groups_only: false,
                level: VerbosityLevel::Normal,
            },
        )
        .map_err(Error::Error)?;

        Ok(())
    })?;

    handle_preserve(&attributes.mode, || -> CopyResult<()> {
        // The `chmod()` system call that underlies the
        // `fs::set_permissions()` call is unable to change the
        // permissions of a symbolic link. In that case, we just
        // do nothing, since every symbolic link has the same
        // permissions.
        if !dest.is_symlink() {
            fs::set_permissions(dest, source_metadata.permissions()).context(context)?;
            // FIXME: Implement this for windows as well
            #[cfg(feature = "feat_acl")]
            exacl::getfacl(source, None)
                .and_then(|acl| exacl::setfacl(&[dest], &acl, None))
                .map_err(|err| Error::Error(err.to_string()))?;
        }

        Ok(())
    })?;

    handle_preserve(&attributes.timestamps, || -> CopyResult<()> {
        let atime = FileTime::from_last_access_time(&source_metadata);
        let mtime = FileTime::from_last_modification_time(&source_metadata);
        if dest.is_symlink() {
            filetime::set_symlink_file_times(dest, atime, mtime)?;
        } else {
            filetime::set_file_times(dest, atime, mtime)?;
        }

        Ok(())
    })?;

    #[cfg(feature = "feat_selinux")]
    handle_preserve(&attributes.context, || -> CopyResult<()> {
        let context = selinux::SecurityContext::of_path(source, false, false).map_err(|e| {
            format!(
                "failed to get security context of {}: {}",
                source.display(),
                e
            )
        })?;
        if let Some(context) = context {
            context.set_for_path(dest, false, false).map_err(|e| {
                format!(
                    "failed to set security context for {}: {}",
                    dest.display(),
                    e
                )
            })?;
        }

        Ok(())
    })?;

    handle_preserve(&attributes.xattr, || -> CopyResult<()> {
        #[cfg(unix)]
        {
            let xattrs = xattr::list(source)?;
            for attr in xattrs {
                if let Some(attr_value) = xattr::get(source, attr.clone())? {
                    xattr::set(dest, attr, &attr_value[..])?;
                }
            }
        }
        #[cfg(not(unix))]
        {
            // The documentation for GNU cp states:
            //
            // > Try to preserve SELinux security context and
            // > extended attributes (xattr), but ignore any failure
            // > to do that and print no corresponding diagnostic.
            //
            // so we simply do nothing here.
            //
            // TODO Silently ignore failures in the `#[cfg(unix)]`
            // block instead of terminating immediately on errors.
        }

        Ok(())
    })?;

    Ok(())
}

fn symlink_file(
    source: &Path,
    dest: &Path,
    context: &str,
    symlinked_files: &mut HashSet<FileInformation>,
) -> CopyResult<()> {
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(source, dest).context(context)?;
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(source, dest).context(context)?;
    }
    if let Ok(file_info) = FileInformation::from_path(dest, false) {
        symlinked_files.insert(file_info);
    }
    Ok(())
}

fn context_for(src: &Path, dest: &Path) -> String {
    format!("{} -> {}", src.quote(), dest.quote())
}

/// Implements a simple backup copy for the destination file.
/// TODO: for the backup, should this function be replaced by `copy_file(...)`?
fn backup_dest(dest: &Path, backup_path: &Path) -> CopyResult<PathBuf> {
    fs::copy(dest, backup_path)?;
    Ok(backup_path.into())
}

/// Decide whether source and destination files are the same and
/// copying is forbidden.
///
/// Copying to the same file is only allowed if both `--backup` and
/// `--force` are specified and the file is a regular file.
fn is_forbidden_copy_to_same_file(
    source: &Path,
    dest: &Path,
    options: &Options,
    source_in_command_line: bool,
) -> bool {
    // TODO To match the behavior of GNU cp, we also need to check
    // that the file is a regular file.
    let dereference_to_compare =
        options.dereference(source_in_command_line) || !source.is_symlink();
    paths_refer_to_same_file(source, dest, dereference_to_compare)
        && !(options.force() && options.backup != BackupMode::NoBackup)
}

/// Back up, remove, or leave intact the destination file, depending on the options.
fn handle_existing_dest(
    source: &Path,
    dest: &Path,
    options: &Options,
    source_in_command_line: bool,
) -> CopyResult<()> {
    // Disallow copying a file to itself, unless `--force` and
    // `--backup` are both specified.
    if is_forbidden_copy_to_same_file(source, dest, options, source_in_command_line) {
        return Err(format!("{} and {} are the same file", source.quote(), dest.quote()).into());
    }

    options.overwrite.verify(dest)?;

    let backup_path = backup_control::get_backup_path(options.backup, dest, &options.backup_suffix);
    if let Some(backup_path) = backup_path {
        if paths_refer_to_same_file(source, &backup_path, true) {
            return Err(format!(
                "backing up {} might destroy source;  {} not copied",
                dest.quote(),
                source.quote()
            )
            .into());
        } else {
            backup_dest(dest, &backup_path)?;
        }
    }

    match options.overwrite {
        // FIXME: print that the file was removed if --verbose is enabled
        OverwriteMode::Clobber(ClobberMode::Force) => {
            if fs::metadata(dest)?.permissions().readonly() {
                fs::remove_file(dest)?;
            }
        }
        OverwriteMode::Clobber(ClobberMode::RemoveDestination) => {
            fs::remove_file(dest)?;
        }
        _ => (),
    };

    Ok(())
}

/// Decide whether the given path exists.
fn file_or_link_exists(path: &Path) -> bool {
    // Using `Path.exists()` or `Path.try_exists()` is not sufficient,
    // because if `path` is a symbolic link and there are too many
    // levels of symbolic link, then those methods will return false
    // or an OS error.
    path.symlink_metadata().is_ok()
}

/// Zip the ancestors of a source path and destination path.
///
/// # Examples
///
/// ```rust,ignore
/// let actual = aligned_ancestors(&Path::new("a/b/c"), &Path::new("d/a/b/c"));
/// let expected = vec![
///     (Path::new("a"), Path::new("d/a")),
///     (Path::new("a/b"), Path::new("d/a/b")),
/// ];
/// assert_eq!(actual, expected);
/// ```
fn aligned_ancestors<'a>(source: &'a Path, dest: &'a Path) -> Vec<(&'a Path, &'a Path)> {
    // Collect the ancestors of each. For example, if `source` is
    // "a/b/c", then the ancestors are "a/b/c", "a/b", "a/", and "".
    let source_ancestors: Vec<&Path> = source.ancestors().collect();
    let dest_ancestors: Vec<&Path> = dest.ancestors().collect();

    // For this particular application, we don't care about the null
    // path "" and we don't care about the full path (e.g. "a/b/c"),
    // so we exclude those.
    let n = source_ancestors.len();
    let source_ancestors = &source_ancestors[1..n - 1];

    // Get the matching number of elements from the ancestors of the
    // destination path (for example, get "d/a" and "d/a/b").
    let k = source_ancestors.len();
    let dest_ancestors = &dest_ancestors[1..1 + k];

    // Now we have two slices of the same length, so we zip them.
    let mut result = vec![];
    for (x, y) in source_ancestors
        .iter()
        .rev()
        .zip(dest_ancestors.iter().rev())
    {
        result.push((*x, *y));
    }
    result
}

/// Copy the a file from `source` to `dest`. `source` will be dereferenced if
/// `options.dereference` is set to true. `dest` will be dereferenced only if
/// the source was not a symlink.
///
/// Behavior when copying to existing files is contingent on the
/// `options.overwrite` mode. If a file is skipped, the return type
/// should be `Error:Skipped`
///
/// The original permissions of `source` will be copied to `dest`
/// after a successful copy.
fn copy_file(
    progress_bar: &Option<ProgressBar>,
    source: &Path,
    dest: &Path,
    options: &Options,
    symlinked_files: &mut HashSet<FileInformation>,
    source_in_command_line: bool,
) -> CopyResult<()> {
    if options.update && options.overwrite == OverwriteMode::Interactive(ClobberMode::Standard) {
        // `cp -i --update old new` when `new` exists doesn't copy anything
        // and exit with 0
        return Ok(());
    }

    // Fail if dest is a dangling symlink or a symlink this program created previously
    if dest.is_symlink() {
        if FileInformation::from_path(dest, false)
            .map(|info| symlinked_files.contains(&info))
            .unwrap_or(false)
        {
            return Err(Error::Error(format!(
                "will not copy '{}' through just-created symlink '{}'",
                source.display(),
                dest.display()
            )));
        }
        let copy_contents = options.dereference(source_in_command_line) || !source.is_symlink();
        if copy_contents
            && !dest.exists()
            && !matches!(
                options.overwrite,
                OverwriteMode::Clobber(ClobberMode::RemoveDestination)
            )
        {
            return Err(Error::Error(format!(
                "not writing through dangling symlink '{}'",
                dest.display()
            )));
        }
    }

    if file_or_link_exists(dest) {
        handle_existing_dest(source, dest, options, source_in_command_line)?;
    }

    if options.verbose {
        if let Some(pb) = progress_bar {
            // Suspend (hide) the progress bar so the println won't overlap with the progress bar.
            pb.suspend(|| {
                if options.parents {
                    // For example, if copying file `a/b/c` and its parents
                    // to directory `d/`, then print
                    //
                    //     a -> d/a
                    //     a/b -> d/a/b
                    //
                    for (x, y) in aligned_ancestors(source, dest) {
                        println!("{} -> {}", x.display(), y.display());
                    }
                }

                println!("{}", context_for(source, dest));
            });
        } else {
            if options.parents {
                // For example, if copying file `a/b/c` and its parents
                // to directory `d/`, then print
                //
                //     a -> d/a
                //     a/b -> d/a/b
                //
                for (x, y) in aligned_ancestors(source, dest) {
                    println!("{} -> {}", x.display(), y.display());
                }
            }

            println!("{}", context_for(source, dest));
        }
    }

    // Calculate the context upfront before canonicalizing the path
    let context = context_for(source, dest);
    let context = context.as_str();

    let source_metadata = {
        let result = if options.dereference(source_in_command_line) {
            fs::metadata(source)
        } else {
            fs::symlink_metadata(source)
        };
        result.context(context)?
    };
    let source_file_type = source_metadata.file_type();
    let source_is_symlink = source_file_type.is_symlink();

    #[cfg(unix)]
    let source_is_fifo = source_file_type.is_fifo();
    #[cfg(not(unix))]
    let source_is_fifo = false;

    let dest_permissions = if dest.exists() {
        dest.symlink_metadata().context(context)?.permissions()
    } else {
        #[allow(unused_mut)]
        let mut permissions = source_metadata.permissions();
        #[cfg(unix)]
        {
            use uucore::mode::get_umask;

            let mut mode = permissions.mode();

            // remove sticky bit, suid and gid bit
            const SPECIAL_PERMS_MASK: u32 = 0o7000;
            mode &= !SPECIAL_PERMS_MASK;

            // apply umask
            mode &= !get_umask();

            permissions.set_mode(mode);
        }
        permissions
    };

    match options.copy_mode {
        CopyMode::Link => {
            if dest.exists() {
                let backup_path =
                    backup_control::get_backup_path(options.backup, dest, &options.backup_suffix);
                if let Some(backup_path) = backup_path {
                    backup_dest(dest, &backup_path)?;
                    fs::remove_file(dest)?;
                }
                if options.overwrite == OverwriteMode::Clobber(ClobberMode::Force) {
                    fs::remove_file(dest)?;
                }
            }
            if options.dereference(source_in_command_line) && source.is_symlink() {
                let resolved =
                    canonicalize(source, MissingHandling::Missing, ResolveMode::Physical).unwrap();
                fs::hard_link(resolved, dest)
            } else {
                fs::hard_link(source, dest)
            }
            .context(context)?;
        }
        CopyMode::Copy => {
            copy_helper(
                source,
                dest,
                options,
                context,
                source_is_symlink,
                source_is_fifo,
                symlinked_files,
            )?;
        }
        CopyMode::SymLink => {
            if dest.exists() && options.overwrite == OverwriteMode::Clobber(ClobberMode::Force) {
                fs::remove_file(dest)?;
            }
            symlink_file(source, dest, context, symlinked_files)?;
        }
        CopyMode::Update => {
            if dest.exists() {
                let dest_metadata = fs::symlink_metadata(dest)?;

                let src_time = source_metadata.modified()?;
                let dest_time = dest_metadata.modified()?;
                if src_time <= dest_time {
                    return Ok(());
                } else {
                    copy_helper(
                        source,
                        dest,
                        options,
                        context,
                        source_is_symlink,
                        source_is_fifo,
                        symlinked_files,
                    )?;
                }
            } else {
                copy_helper(
                    source,
                    dest,
                    options,
                    context,
                    source_is_symlink,
                    source_is_fifo,
                    symlinked_files,
                )?;
            }
        }
        CopyMode::AttrOnly => {
            OpenOptions::new()
                .write(true)
                .truncate(false)
                .create(true)
                .open(dest)
                .unwrap();
        }
    };

    // TODO: implement something similar to gnu's lchown
    if !dest.is_symlink() {
        // Here, to match GNU semantics, we quietly ignore an error
        // if a user does not have the correct ownership to modify
        // the permissions of a file.
        //
        // FWIW, the OS will throw an error later, on the write op, if
        // the user does not have permission to write to the file.
        fs::set_permissions(dest, dest_permissions).ok();
    }

    copy_attributes(source, dest, &options.attributes)?;

    if let Some(progress_bar) = progress_bar {
        progress_bar.inc(fs::metadata(source)?.len());
    }

    Ok(())
}

/// Copy the file from `source` to `dest` either using the normal `fs::copy` or a
/// copy-on-write scheme if --reflink is specified and the filesystem supports it.
fn copy_helper(
    source: &Path,
    dest: &Path,
    options: &Options,
    context: &str,
    source_is_symlink: bool,
    source_is_fifo: bool,
    symlinked_files: &mut HashSet<FileInformation>,
) -> CopyResult<()> {
    if options.parents {
        let parent = dest.parent().unwrap_or(dest);
        fs::create_dir_all(parent)?;
    }

    if source.as_os_str() == "/dev/null" {
        /* workaround a limitation of fs::copy
         * https://github.com/rust-lang/rust/issues/79390
         */
        File::create(dest).context(dest.display().to_string())?;
    } else if source_is_fifo && options.recursive && !options.copy_contents {
        #[cfg(unix)]
        copy_fifo(dest, options.overwrite)?;
    } else if source_is_symlink {
        copy_link(source, dest, symlinked_files)?;
    } else {
        copy_on_write(
            source,
            dest,
            options.reflink_mode,
            options.sparse_mode,
            context,
            #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
            source_is_fifo,
        )?;
    }

    Ok(())
}

// "Copies" a FIFO by creating a new one. This workaround is because Rust's
// built-in fs::copy does not handle FIFOs (see rust-lang/rust/issues/79390).
#[cfg(unix)]
fn copy_fifo(dest: &Path, overwrite: OverwriteMode) -> CopyResult<()> {
    if dest.exists() {
        overwrite.verify(dest)?;
        fs::remove_file(dest)?;
    }

    let name = CString::new(dest.as_os_str().as_bytes()).unwrap();
    let err = unsafe { mkfifo(name.as_ptr(), 0o666) };
    if err == -1 {
        return Err(format!("cannot create fifo {}: File exists", dest.quote()).into());
    }
    Ok(())
}

fn copy_link(
    source: &Path,
    dest: &Path,
    symlinked_files: &mut HashSet<FileInformation>,
) -> CopyResult<()> {
    // Here, we will copy the symlink itself (actually, just recreate it)
    let link = fs::read_link(source)?;
    let dest: Cow<'_, Path> = if dest.is_dir() {
        match source.file_name() {
            Some(name) => dest.join(name).into(),
            None => crash!(
                EXIT_ERR,
                "cannot stat {}: No such file or directory",
                source.quote()
            ),
        }
    } else {
        // we always need to remove the file to be able to create a symlink,
        // even if it is writeable.
        if dest.is_symlink() || dest.is_file() {
            fs::remove_file(dest)?;
        }
        dest.into()
    };
    symlink_file(&link, &dest, &context_for(&link, &dest), symlinked_files)
}

/// Generate an error message if `target` is not the correct `target_type`
pub fn verify_target_type(target: &Path, target_type: &TargetType) -> CopyResult<()> {
    match (target_type, target.is_dir()) {
        (&TargetType::Directory, false) => {
            Err(format!("target: {} is not a directory", target.quote()).into())
        }
        (&TargetType::File, true) => Err(format!(
            "cannot overwrite directory {} with non-directory",
            target.quote()
        )
        .into()),
        _ => Ok(()),
    }
}

/// Remove the `root` prefix from `source` and prefix it with `target`
/// to create a file that is local to `target`
/// # Examples
///
/// ```ignore
/// assert!(uu_cp::localize_to_target(
///     &Path::new("a/source/"),
///     &Path::new("a/source/c.txt"),
///     &Path::new("target/"),
/// ).unwrap() == Path::new("target/c.txt"))
/// ```
pub fn localize_to_target(root: &Path, source: &Path, target: &Path) -> CopyResult<PathBuf> {
    let local_to_root = source.strip_prefix(root)?;
    Ok(target.join(local_to_root))
}

/// Get the total size of a slice of files and directories.
///
/// This function is much like the `du` utility, by recursively getting the sizes of files in directories.
/// Files are not deduplicated when appearing in multiple sources. If `recursive` is set to `false`, the
/// directories in `paths` will be ignored.
fn disk_usage(paths: &[PathBuf], recursive: bool) -> io::Result<u64> {
    let mut total = 0;
    for p in paths {
        let md = fs::metadata(p)?;
        if md.file_type().is_dir() {
            if recursive {
                total += disk_usage_directory(p)?;
            }
        } else {
            total += md.len();
        }
    }
    Ok(total)
}

/// A helper for `disk_usage` specialized for directories.
fn disk_usage_directory(p: &Path) -> io::Result<u64> {
    let mut total = 0;

    for entry in fs::read_dir(p)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            total += disk_usage_directory(&entry.path())?;
        } else {
            total += entry.metadata()?.len();
        }
    }

    Ok(total)
}

#[cfg(test)]
mod tests {

    use crate::{aligned_ancestors, localize_to_target};
    use std::path::Path;

    #[test]
    fn test_cp_localize_to_target() {
        let root = Path::new("a/source/");
        let source = Path::new("a/source/c.txt");
        let target = Path::new("target/");
        let actual = localize_to_target(root, source, target).unwrap();
        let expected = Path::new("target/c.txt");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_aligned_ancestors() {
        let actual = aligned_ancestors(Path::new("a/b/c"), Path::new("d/a/b/c"));
        let expected = vec![
            (Path::new("a"), Path::new("d/a")),
            (Path::new("a/b"), Path::new("d/a/b")),
        ];
        assert_eq!(actual, expected);
    }
}
