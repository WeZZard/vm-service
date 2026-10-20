//! The image catalog's real inputs: `images/*/line.conf` under the
//! pilot-images checkout (IC-2), and the strict builder that executes them
//! (IC-4).

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    BuiltLines, CatalogFailure, CatalogSource, FileIdentity, IdentityEntry, ImageLine,
    InputIdentity, ResourceDefaults,
};
use crate::application_catalog::MAX_IMAGES;

/// The `images/` folder of a pilot-images checkout.
#[derive(Debug, Clone)]
pub struct FileSource {
    images: PathBuf,
}

impl FileSource {
    /// A source over `images`, normally `<pilot>/images`.
    pub fn new(images: PathBuf) -> Self {
        Self { images }
    }
}

impl CatalogSource for FileSource {
    fn identity(&self) -> InputIdentity {
        identity(&self.images)
    }

    fn build(&self, identity: &InputIdentity) -> Result<BuiltLines, CatalogFailure> {
        build(&self.images, identity)
    }

    fn watch_root(&self) -> Option<PathBuf> {
        Some(self.images.clone())
    }
}

/// Compute the input identity of `images` (IC-2).
///
/// Reads one folder, stats each `line.conf`, and hashes at most
/// [`MAX_IMAGES`] small files. Executes nothing. Folders without a
/// `line.conf` and entries that are not folders are not inputs, as at
/// startup before hot reload.
pub fn identity(images: &Path) -> InputIdentity {
    let unreadable = |reason: String| InputIdentity {
        entries: Vec::new(),
        unreadable: Some(reason),
    };
    let read = match std::fs::read_dir(images) {
        Ok(read) => read,
        Err(error) => return unreadable(format!("{}: {error}", images.display())),
    };
    let mut entries = Vec::new();
    for entry in read {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return unreadable(format!("{}: {error}", images.display())),
        };
        let directory = entry.path();
        let metadata = match std::fs::metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) => return unreadable(format!("{}: {error}", directory.display())),
        };
        if !metadata.is_dir() {
            continue;
        }
        let path = directory.join("line.conf");
        let file = match std::fs::metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => FileIdentity::Unreadable(error.to_string()),
            Ok(metadata) if !metadata.is_file() => FileIdentity::NonRegular,
            Ok(metadata) => FileIdentity::Regular {
                inode: metadata.ino(),
                size: metadata.size(),
                mtime_ns: i128::from(metadata.mtime()) * 1_000_000_000
                    + i128::from(metadata.mtime_nsec()),
                sha256: None,
            },
        };
        entries.push(IdentityEntry {
            image: entry.file_name().to_string_lossy().into_owned(),
            path,
            file,
        });
    }
    entries.sort_by(|left, right| left.image.cmp(&right.image));
    if entries.len() <= MAX_IMAGES {
        for entry in &mut entries {
            if let FileIdentity::Regular { sha256, .. } = &mut entry.file {
                match std::fs::read(&entry.path) {
                    Ok(bytes) => *sha256 = Some(format!("{:x}", Sha256::digest(&bytes))),
                    Err(error) => entry.file = FileIdentity::Unreadable(error.to_string()),
                }
            }
        }
    }
    InputIdentity {
        entries,
        unreadable: None,
    }
}

/// Build every line `identity` names, strictly (IC-4).
///
/// Every rule of the startup build applies, and any one failing fails the
/// whole build: an unreadable folder, no lines, more than [`MAX_IMAGES`]
/// lines, a non-regular or unreadable `line.conf`, a parse or source failure
/// (with the 15-second timeout), an invalid image or base name, a kind other
/// than `macos` or `linux`, and a resource default that is not an integer.
pub fn build(images: &Path, identity: &InputIdentity) -> Result<BuiltLines, CatalogFailure> {
    let folder = || Some(images.display().to_string());
    if let Some(reason) = &identity.unreadable {
        return Err(CatalogFailure::new(
            folder(),
            format!("image line folder unreadable: {reason}"),
        ));
    }
    if identity.entries.is_empty() {
        return Err(CatalogFailure::new(folder(), "no configured image lines"));
    }
    if identity.entries.len() > MAX_IMAGES {
        return Err(CatalogFailure::new(
            folder(),
            format!(
                "too many image lines: {} (at most {MAX_IMAGES})",
                identity.entries.len()
            ),
        ));
    }
    let mut built = BuiltLines {
        lines: BTreeMap::new(),
        credentials: Default::default(),
    };
    for entry in &identity.entries {
        let file = Some(entry.path.display().to_string());
        let fail = |reason: String| CatalogFailure::new(file.clone(), reason);
        match &entry.file {
            FileIdentity::NonRegular => {
                return Err(fail("nonregular line configuration".into()));
            }
            FileIdentity::Unreadable(reason) => {
                return Err(fail(format!("line configuration unreadable: {reason}")));
            }
            FileIdentity::Regular { .. } => {}
        }
        let conf = crate::lines::read_line_conf(&entry.path).map_err(fail)?;
        let text = |key: &str| {
            conf.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let kind = text("LINE_KIND");
        let base_vm = text("BASE_VM");
        for name in [&entry.image, &base_vm] {
            if !valid_name(name) {
                return Err(fail(format!(
                    "invalid image/base association: {:?} -> {:?}",
                    entry.image, base_vm
                )));
            }
        }
        if kind != "linux" && kind != "macos" {
            return Err(fail(format!(
                "invalid association OS: LINE_KIND {kind:?} is neither macos nor linux"
            )));
        }
        let (cpu, memory_mb, disk_gb) = crate::lines::resource_defaults(&conf).map_err(fail)?;
        let work_vm = Some(text("WORK_VM")).filter(|value| !value.is_empty());
        built.lines.insert(
            entry.image.clone(),
            ImageLine {
                kind,
                base_vm,
                work_vm,
                clone_prefix: text("CLONE_PREFIX"),
                guest_user: text("GUEST_USER"),
                defaults: ResourceDefaults {
                    cpu: Some(cpu),
                    memory_mb: Some(memory_mb),
                    disk_gb,
                },
                source: entry.path.to_string_lossy().into_owned(),
            },
        );
        built
            .credentials
            .insert(entry.image.clone(), text("GUEST_PASS"));
    }
    Ok(built)
}

/// An image or base name: 1 to 128 characters, starting alphanumeric, then
/// alphanumerics, `_`, `.` and `-`.
pub fn valid_name(value: &str) -> bool {
    if value.is_empty() || value.len() > 128 {
        return false;
    }
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}
