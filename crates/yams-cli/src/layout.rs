use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum DirsOverride {
    #[default]
    Absent,
    SetEmpty {
        variable: &'static str,
    },
    NonEmpty(OsString),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ResolvedDirsOverride {
    #[default]
    Absent,
    SetEmpty {
        variable: &'static str,
    },
    NonEmpty(Vec<PathBuf>),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Environment {
    home: Option<OsString>,
    dirs: DirsOverride,
    allow_net: Option<OsString>,
    no_service: Option<OsString>,
    service_socket: Option<OsString>,
}

impl Environment {
    pub fn resolve<I, K, V>(variables: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        let variables: HashMap<OsString, OsString> = variables
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect();
        Self {
            home: resolve_variable(&variables, "YAMS_HOME"),
            dirs: resolve_dirs(&variables),
            allow_net: resolve_variable(&variables, "YAMS_ALLOW_NET"),
            no_service: resolve_variable(&variables, "YAMS_NO_SERVICE"),
            service_socket: resolve_variable(&variables, "YAMS_SERVICE_SOCKET"),
        }
    }

    pub fn home(&self) -> Option<&OsStr> {
        self.home.as_deref()
    }

    pub fn dirs(&self) -> Option<&OsStr> {
        match &self.dirs {
            DirsOverride::NonEmpty(value) => Some(value),
            DirsOverride::Absent | DirsOverride::SetEmpty { .. } => None,
        }
    }

    pub fn dirs_override(&self) -> &DirsOverride {
        &self.dirs
    }

    pub fn allow_net(&self) -> bool {
        self.allow_net.as_deref() == Some(OsStr::new("1"))
    }

    pub fn no_service(&self) -> bool {
        self.no_service.as_deref() == Some(OsStr::new("1"))
    }

    pub fn service_socket(&self) -> Option<&OsStr> {
        self.service_socket.as_deref()
    }
}

fn resolve_dirs(variables: &HashMap<OsString, OsString>) -> DirsOverride {
    let value = variables.get(OsStr::new("YAMS_DIRS"));
    match nonempty(value) {
        Some(value) => DirsOverride::NonEmpty(value.clone()),
        None if value.is_some() => DirsOverride::SetEmpty {
            variable: "YAMS_DIRS",
        },
        None => DirsOverride::Absent,
    }
}

fn resolve_variable(
    variables: &HashMap<OsString, OsString>,
    variable: &'static str,
) -> Option<OsString> {
    nonempty(variables.get(OsStr::new(variable))).cloned()
}

fn nonempty(value: Option<&OsString>) -> Option<&OsString> {
    value.filter(|value| !value.is_empty())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    MacOs,
    Unsupported(&'static str),
}

impl Platform {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Unsupported(std::env::consts::OS)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeInputs {
    pub cwd: PathBuf,
    pub home: PathBuf,
    pub temporary_directory: PathBuf,
    pub uid: u32,
    pub platform: Platform,
}

impl RuntimeInputs {
    pub fn current() -> Result<Self, LayoutError> {
        let cwd = std::env::current_dir().map_err(LayoutError::CurrentDirectory)?;
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_default();
        Ok(Self {
            cwd,
            home,
            temporary_directory: std::env::temp_dir(),
            uid: rustix::process::getuid().as_raw(),
            platform: Platform::current(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeLayout {
    pub cwd: PathBuf,
    pub application_support_dir: PathBuf,
    pub query_log: PathBuf,
    pub cache_dir: PathBuf,
    pub store_dir: PathBuf,
    pub indexes_dir: PathBuf,
    pub vectors_path: PathBuf,
    pub model_cache_dir: PathBuf,
    pub model_lock_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub service_socket: PathBuf,
    pub corpus_dirs: ResolvedDirsOverride,
}

impl RuntimeLayout {
    pub fn resolve(environment: &Environment, inputs: &RuntimeInputs) -> Result<Self, LayoutError> {
        require_absolute("current directory", &inputs.cwd)?;
        let cwd = resolve_compatible(&inputs.cwd, &inputs.cwd, &inputs.home)?;
        let explicit_home = environment
            .home()
            .map(PathBuf::from)
            .map(|path| resolve_compatible(&cwd, &path, &inputs.home))
            .transpose()?;
        let (application_support_dir, cache_dir, runtime_dir) = if let Some(base) = explicit_home {
            (base.clone(), base.clone(), base)
        } else {
            if let Platform::Unsupported(platform) = inputs.platform {
                return Err(LayoutError::UnsupportedPlatform(platform));
            }
            if inputs.home.as_os_str().is_empty() {
                return Err(LayoutError::MissingHome);
            }
            require_absolute("HOME", &inputs.home)?;
            require_absolute("TMPDIR", &inputs.temporary_directory)?;
            let home = resolve_compatible(&cwd, &inputs.home, &inputs.home)?;
            let temporary_directory =
                resolve_compatible(&cwd, &inputs.temporary_directory, &inputs.home)?;
            (
                home.join("Library")
                    .join("Application Support")
                    .join("yams"),
                home.join("Library/Caches/yams"),
                temporary_directory.join(format!("yams-{}", inputs.uid)),
            )
        };
        let store_dir = cache_dir.join("rust-v1");
        let service_socket = environment
            .service_socket()
            .map(|path| resolve_compatible(&cwd, Path::new(path), &inputs.home))
            .transpose()?
            .unwrap_or_else(|| runtime_dir.join("service.sock"));
        let corpus_dirs = match environment.dirs_override() {
            DirsOverride::Absent => ResolvedDirsOverride::Absent,
            DirsOverride::SetEmpty { variable } => ResolvedDirsOverride::SetEmpty { variable },
            DirsOverride::NonEmpty(paths) => ResolvedDirsOverride::NonEmpty(
                std::env::split_paths(paths)
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(|path| resolve_compatible(&cwd, &path, &inputs.home))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        };
        Ok(Self {
            cwd,
            query_log: application_support_dir.join("queries.jsonl"),
            indexes_dir: store_dir.join("indexes"),
            vectors_path: store_dir.join("vectors.sqlite3"),
            model_cache_dir: store_dir.join("models"),
            model_lock_dir: store_dir.join("locks"),
            application_support_dir,
            cache_dir,
            store_dir,
            runtime_dir,
            service_socket,
            corpus_dirs,
        })
    }
}

pub(crate) fn resolve_project_path(
    explicit: Option<&Path>,
    inputs: &RuntimeInputs,
) -> Result<PathBuf, LayoutError> {
    require_absolute("current directory", &inputs.cwd)?;
    let cwd = resolve_compatible(&inputs.cwd, &inputs.cwd, &inputs.home)?;
    let explicit = explicit
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| resolve_compatible(&cwd, path, &inputs.home))
        .transpose()?;
    yams_core::project_root(explicit.as_deref(), &cwd).map_err(LayoutError::Project)
}

fn require_absolute(name: &'static str, path: &Path) -> Result<(), LayoutError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(LayoutError::RelativeInput(name))
    }
}

fn resolve_compatible(cwd: &Path, path: &Path, home: &Path) -> Result<PathBuf, LayoutError> {
    let expanded = expand_tilde(path, home)?;
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    resolve_absolute_weak(&absolute, 0)
}

fn resolve_absolute_weak(absolute: &Path, symlink_depth: usize) -> Result<PathBuf, LayoutError> {
    if symlink_depth > 40 {
        return Err(LayoutError::SymlinkLoop(absolute.to_owned()));
    }
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            Component::RootDir => resolved.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                let candidate = resolved.join(name);
                match std::fs::symlink_metadata(&candidate) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        let target = std::fs::read_link(&candidate).map_err(|source| {
                            LayoutError::ResolvePath {
                                path: candidate.clone(),
                                source,
                            }
                        })?;
                        let target = if target.is_absolute() {
                            target
                        } else {
                            resolved.join(target)
                        };
                        resolved = resolve_absolute_weak(&target, symlink_depth + 1)?;
                    }
                    Ok(_) => resolved.push(name),
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                        ) =>
                    {
                        resolved.push(name);
                    }
                    Err(source) => {
                        return Err(LayoutError::ResolvePath {
                            path: candidate,
                            source,
                        });
                    }
                }
            }
        }
    }
    Ok(resolved)
}

fn expand_tilde(path: &Path, home: &Path) -> Result<PathBuf, LayoutError> {
    let mut components = path.components();
    if matches!(components.next(), Some(Component::Normal(name)) if name == OsStr::new("~")) {
        if home.as_os_str().is_empty() {
            return Err(LayoutError::MissingHome);
        }
        require_absolute("HOME", home)?;
        return Ok(home.join(components.as_path()));
    }
    Ok(path.to_owned())
}

#[derive(Debug, Error)]
pub enum LayoutError {
    #[error("could not resolve the current directory: {0}")]
    CurrentDirectory(#[source] io::Error),
    #[error("HOME is unset or empty")]
    MissingHome,
    #[error("{0} must be an absolute path")]
    RelativeInput(&'static str),
    #[error("cannot resolve path {path}: {source}")]
    ResolvePath {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("too many symbolic links while resolving {0}")]
    SymlinkLoop(PathBuf),
    #[error("could not resolve the selected project: {0}")]
    Project(#[source] yams_core::DiscoveryError),
    #[error("Yams's default runtime layout is not defined for {0}")]
    UnsupportedPlatform(&'static str),
}
