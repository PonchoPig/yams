use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

#[derive(Clone, Debug, Default)]
pub struct Discovery {
    pub home: Option<PathBuf>,
    /// Absolute override paths. Nonempty overrides replace default discovery.
    pub override_dirs: Vec<PathBuf>,
    /// Other project roots that must not share this root's Claude private slug.
    pub known_roots: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CorpusKind {
    Shared,
    Private,
    Override,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Corpus {
    pub(crate) path: PathBuf,
    pub(crate) kind: CorpusKind,
    pub(crate) validation: CorpusValidation,
}

impl Corpus {
    /// Returns the canonical corpus root approved when this value was built.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the discovery provenance of this corpus.
    pub fn kind(&self) -> CorpusKind {
        self.kind
    }

    /// Validates an explicitly supplied corpus for safe descriptor-relative scanning.
    pub fn validated(path: &Path, kind: CorpusKind) -> Result<Self, DiscoveryError> {
        let canonical = canonical_directory(path)?;
        let base = canonical.parent().unwrap_or(&canonical);
        let canonical_base = canonical_directory(base)?;
        validated_corpus(canonical, canonical_base, kind)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CorpusValidation {
    pub(crate) base: PathBuf,
    pub(crate) relative: PathBuf,
    pub(crate) expected_base: NodeIdentity,
    pub(crate) expected_root: NodeIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NodeIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

impl NodeIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscoveryNoteKind {
    EscapesBase,
    Unreadable,
    NotDirectory,
    RelativeOverride,
    InvalidPath,
    PrivateSlugCollision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryNote {
    pub path: PathBuf,
    pub kind: DiscoveryNoteKind,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiscoveryReport {
    /// Valid corpora, in configured discovery order.
    pub corpora: Vec<Corpus>,
    /// Nonfatal rejected or unreadable discovery candidates.
    pub notes: Vec<DiscoveryNote>,
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("cannot resolve {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot inspect {path}: {source}")]
    Inspect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("corpus path is not a directory: {path}")]
    NotDirectory { path: PathBuf },
    #[error("project root is not valid UTF-8: {path}")]
    NonUtf8Root { path: PathBuf },
    #[error("{kind:?} corpus {path} resolves to {resolved}, which is outside its base {base}")]
    EscapesBase {
        kind: CorpusKind,
        path: PathBuf,
        resolved: PathBuf,
        base: PathBuf,
    },
}

pub fn project_root(explicit: Option<&Path>, cwd: &Path) -> Result<PathBuf, DiscoveryError> {
    if let Some(explicit) = explicit {
        let selected = if explicit.is_absolute() {
            explicit.to_path_buf()
        } else {
            cwd.join(explicit)
        };
        return canonical_directory(&selected);
    }

    let canonical_cwd = canonical_directory(cwd)?;
    for ancestor in canonical_cwd.ancestors() {
        let marker = ancestor.join(".git");
        match fs::metadata(&marker) {
            Ok(metadata) if metadata.is_file() || metadata.is_dir() => {
                return canonical_directory(ancestor);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(DiscoveryError::Inspect {
                    path: marker,
                    source,
                });
            }
        }
    }
    Ok(canonical_cwd)
}

pub fn discover_corpora(
    root: &Path,
    discovery: &Discovery,
) -> Result<DiscoveryReport, DiscoveryError> {
    let canonical_root = canonical_directory(root)?;
    let mut report = DiscoveryReport::default();
    let mut seen = HashSet::new();

    if !discovery.override_dirs.is_empty() {
        for configured in &discovery.override_dirs {
            if !configured.is_absolute() {
                report.notes.push(DiscoveryNote {
                    path: configured.clone(),
                    kind: DiscoveryNoteKind::RelativeOverride,
                    detail: "override corpus paths must be absolute".to_owned(),
                });
                continue;
            }
            discover_candidate(
                configured.clone(),
                None,
                CorpusKind::Override,
                false,
                &mut report,
                &mut seen,
            );
        }
        return Ok(report);
    }

    discover_candidate(
        canonical_root.join(".agents/memory"),
        Some(canonical_root.clone()),
        CorpusKind::Shared,
        true,
        &mut report,
        &mut seen,
    );

    if let Some(home) = &discovery.home {
        let Some(slug) = claude_private_slug(&canonical_root) else {
            report.notes.push(DiscoveryNote {
                path: canonical_root,
                kind: DiscoveryNoteKind::InvalidPath,
                detail: "Claude private-memory discovery requires a UTF-8 project root".to_owned(),
            });
            return Ok(report);
        };
        let base = home.join(".claude/projects").join(&slug);
        let private = base.join("memory");
        let collisions = colliding_private_roots(&canonical_root, &discovery.known_roots);
        if collisions.is_empty() {
            discover_candidate(
                private,
                Some(base),
                CorpusKind::Private,
                true,
                &mut report,
                &mut seen,
            );
        } else {
            report.notes.push(DiscoveryNote {
                path: private,
                kind: DiscoveryNoteKind::PrivateSlugCollision,
                detail: format!(
                    "Claude private slug is shared with {}",
                    collisions
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
    }

    Ok(report)
}

/// Compatibility wrapper returning only valid corpora.
///
/// This intentionally discards nonfatal [`DiscoveryReport::notes`]. Production
/// and diagnostic callers should use [`discover_corpora`] and surface its notes.
pub fn corpora_for(root: &Path, discovery: &Discovery) -> Result<Vec<Corpus>, DiscoveryError> {
    Ok(discover_corpora(root, discovery)?.corpora)
}

fn discover_candidate(
    candidate: PathBuf,
    strict_base: Option<PathBuf>,
    kind: CorpusKind,
    missing_is_quiet: bool,
    report: &mut DiscoveryReport,
    seen: &mut HashSet<PathBuf>,
) {
    let named = match fs::symlink_metadata(&candidate) {
        Ok(metadata) => metadata,
        Err(error) if missing_is_quiet && missing_default_error(&error) => return,
        Err(error) => {
            report.notes.push(DiscoveryNote {
                path: candidate,
                kind: DiscoveryNoteKind::Unreadable,
                detail: format!("cannot inspect corpus path: {error}"),
            });
            return;
        }
    };
    if !named.is_dir() && !named.file_type().is_symlink() {
        report.notes.push(DiscoveryNote {
            path: candidate,
            kind: DiscoveryNoteKind::NotDirectory,
            detail: "corpus path is not a directory".to_owned(),
        });
        return;
    }

    let canonical = match fs::canonicalize(&candidate) {
        Ok(canonical) => canonical,
        Err(error) => {
            report.notes.push(DiscoveryNote {
                path: candidate,
                kind: DiscoveryNoteKind::Unreadable,
                detail: format!("cannot resolve corpus path: {error}"),
            });
            return;
        }
    };
    let root_metadata = match fs::metadata(&canonical) {
        Ok(metadata) if metadata.is_dir() => metadata,
        Ok(_) => {
            report.notes.push(DiscoveryNote {
                path: candidate,
                kind: DiscoveryNoteKind::NotDirectory,
                detail: format!("corpus resolves to non-directory {}", canonical.display()),
            });
            return;
        }
        Err(error) => {
            report.notes.push(DiscoveryNote {
                path: candidate,
                kind: DiscoveryNoteKind::Unreadable,
                detail: format!(
                    "cannot inspect resolved corpus {}: {error}",
                    canonical.display()
                ),
            });
            return;
        }
    };
    if kind == CorpusKind::Override && is_filesystem_root(&canonical) {
        report.notes.push(DiscoveryNote {
            path: candidate,
            kind: DiscoveryNoteKind::EscapesBase,
            detail: "override corpus cannot be the filesystem root".to_owned(),
        });
        return;
    }
    if kind == CorpusKind::Override && named.file_type().is_symlink() {
        let Some(configured_parent) = candidate.parent() else {
            report.notes.push(DiscoveryNote {
                path: candidate,
                kind: DiscoveryNoteKind::EscapesBase,
                detail: "override symlink has no confinement parent".to_owned(),
            });
            return;
        };
        match canonical_directory(configured_parent) {
            Ok(parent) if canonical.starts_with(&parent) && canonical != parent => {}
            Ok(parent) => {
                report.notes.push(DiscoveryNote {
                    path: candidate,
                    kind: DiscoveryNoteKind::EscapesBase,
                    detail: format!(
                        "override symlink resolves to {}, outside configured parent {}",
                        canonical.display(),
                        parent.display()
                    ),
                });
                return;
            }
            Err(error) => {
                report.notes.push(DiscoveryNote {
                    path: candidate,
                    kind: DiscoveryNoteKind::Unreadable,
                    detail: format!("cannot resolve configured override parent: {error}"),
                });
                return;
            }
        }
    }
    let base = if kind == CorpusKind::Override {
        canonical.clone()
    } else {
        let Some(base) = strict_base else {
            report.notes.push(DiscoveryNote {
                path: candidate,
                kind: DiscoveryNoteKind::InvalidPath,
                detail: "corpus path has no confinement parent".to_owned(),
            });
            return;
        };
        base
    };
    let canonical_base = match canonical_directory(&base) {
        Ok(base) => base,
        Err(error) => {
            report.notes.push(DiscoveryNote {
                path: candidate,
                kind: DiscoveryNoteKind::Unreadable,
                detail: format!(
                    "cannot validate confinement base {}: {error}",
                    base.display()
                ),
            });
            return;
        }
    };
    if (canonical == canonical_base && kind != CorpusKind::Override)
        || !canonical.starts_with(&canonical_base)
    {
        report.notes.push(DiscoveryNote {
            path: candidate,
            kind: DiscoveryNoteKind::EscapesBase,
            detail: format!(
                "corpus resolves to {}, outside confinement base {}",
                canonical.display(),
                canonical_base.display()
            ),
        });
        return;
    }
    if !seen.insert(canonical.clone()) {
        return;
    }

    let base_metadata = match fs::metadata(&canonical_base) {
        Ok(metadata) => metadata,
        Err(error) => {
            report.notes.push(DiscoveryNote {
                path: candidate,
                kind: DiscoveryNoteKind::Unreadable,
                detail: format!("cannot pin confinement base: {error}"),
            });
            return;
        }
    };
    let relative = match safe_relative(&canonical, &canonical_base) {
        Some(relative) => relative,
        None => {
            report.notes.push(DiscoveryNote {
                path: candidate,
                kind: DiscoveryNoteKind::InvalidPath,
                detail: "resolved corpus has an unsafe relative path".to_owned(),
            });
            return;
        }
    };
    report.corpora.push(Corpus {
        path: canonical,
        kind,
        validation: CorpusValidation {
            base: canonical_base,
            relative,
            expected_base: NodeIdentity::from_metadata(&base_metadata),
            expected_root: NodeIdentity::from_metadata(&root_metadata),
        },
    });
}

fn validated_corpus(
    canonical: PathBuf,
    canonical_base: PathBuf,
    kind: CorpusKind,
) -> Result<Corpus, DiscoveryError> {
    if !canonical.starts_with(&canonical_base) {
        return Err(DiscoveryError::EscapesBase {
            kind,
            path: canonical.clone(),
            resolved: canonical,
            base: canonical_base,
        });
    }
    let relative =
        safe_relative(&canonical, &canonical_base).ok_or_else(|| DiscoveryError::EscapesBase {
            kind,
            path: canonical.clone(),
            resolved: canonical.clone(),
            base: canonical_base.clone(),
        })?;
    let base_metadata =
        fs::metadata(&canonical_base).map_err(|source| DiscoveryError::Inspect {
            path: canonical_base.clone(),
            source,
        })?;
    let root_metadata = fs::metadata(&canonical).map_err(|source| DiscoveryError::Inspect {
        path: canonical.clone(),
        source,
    })?;
    Ok(Corpus {
        path: canonical,
        kind,
        validation: CorpusValidation {
            base: canonical_base,
            relative,
            expected_base: NodeIdentity::from_metadata(&base_metadata),
            expected_root: NodeIdentity::from_metadata(&root_metadata),
        },
    })
}

fn canonical_directory(path: &Path) -> Result<PathBuf, DiscoveryError> {
    let canonical = fs::canonicalize(path).map_err(|source| DiscoveryError::Canonicalize {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = fs::metadata(&canonical).map_err(|source| DiscoveryError::Inspect {
        path: canonical.clone(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(DiscoveryError::NotDirectory {
            path: path.to_path_buf(),
        });
    }
    Ok(canonical)
}

fn is_filesystem_root(path: &Path) -> bool {
    path.parent()
        .is_none_or(|parent| parent.as_os_str().is_empty())
}

fn safe_relative(path: &Path, base: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(base).ok()?;
    if !relative
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(relative.to_path_buf())
}

fn missing_default_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::InvalidFilename
    )
}

fn claude_private_slug(path: &Path) -> Option<String> {
    path.to_str().map(claude_slug_text)
}

fn claude_slug_text(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn colliding_private_roots(root: &Path, known_roots: &[PathBuf]) -> Vec<PathBuf> {
    let Some(slug) = claude_private_slug(root) else {
        return Vec::new();
    };
    let mut collisions = HashSet::new();

    if let (Some(parent), Some(name)) = (root.parent(), root.file_name())
        && let Some(this_name) = name.to_str()
        && let Ok(entries) = fs::read_dir(parent)
    {
        let this_component = claude_slug_text(this_name);
        for entry in entries.flatten() {
            let other_file_name = entry.file_name();
            if other_file_name == name {
                continue;
            }
            let Some(other_name) = other_file_name.to_str() else {
                continue;
            };
            if claude_slug_text(other_name) != this_component {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_dir() {
                continue;
            }
            if let Ok(canonical) = fs::canonicalize(entry.path())
                && canonical != root
            {
                collisions.insert(canonical);
            }
        }
    }

    for other in known_roots {
        let Ok(canonical) = fs::canonicalize(other) else {
            continue;
        };
        if canonical == root {
            continue;
        }
        if claude_private_slug(&canonical).as_deref() == Some(slug.as_str()) {
            collisions.insert(canonical);
        }
    }

    let mut collisions = collisions.into_iter().collect::<Vec<_>>();
    collisions.sort();
    collisions
}
