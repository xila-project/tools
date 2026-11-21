//! Seed profiles: the guest application bundles, native application shortcuts
//! and package repositories of an image.

use crate::Result;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Directories of a seeded image, created even when empty.
const DIRECTORIES: &[&str] = &[
    "system/accounts/users",
    "system/accounts/groups",
    "system/executables",
    "system/libraries",
    "system/services",
    "configuration/system/packages",
    "configuration/system/packages/repositories",
    "configuration/system/packages/installed",
    "configuration/applications",
    "configuration/shared/applications/shortcuts",
    "devices/audio",
    "devices/compute",
    "devices/graphics",
    "devices/input",
    "devices/network",
    "devices/random",
    "devices/standard_io",
    "devices/storage",
    "devices/time",
    "applications/executables",
    "applications/libraries",
    "applications/bundles",
    "data/system/logs",
    "data/applications",
    "data/shared",
    "temporary/cache",
    "temporary/sessions",
];

const SHORTCUTS_DIRECTORY: &str = "configuration/shared/applications/shortcuts";
const REPOSITORIES_DIRECTORY: &str = "configuration/system/packages/repositories";
const BUNDLES_DIRECTORY: &str = "applications/bundles";

/// A file seeded by a profile: its file name (without directory) and content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileFile {
    pub name: String,
    pub content: Vec<u8>,
}

/// What a seeded image contains, besides its fixed directory layout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    /// Name reported in messages, such as `native`.
    pub name: String,
    /// Guest applications, by bundle name: `pixel_painter` is seeded from the
    /// bundle directory `xila.pixel-painter`.
    pub applications: Vec<String>,
    /// Native application shortcuts, written to
    /// `/configuration/shared/applications/shortcuts/`. Native applications
    /// are linked into the runtime: only their shortcuts are seeded.
    pub shortcuts: Vec<ProfileFile>,
    /// Package repositories, written to
    /// `/configuration/system/packages/repositories/`.
    pub repositories: Vec<ProfileFile>,
}

/// Paths to load a [`Profile`] from.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProfileSources<'a> {
    /// File listing the guest applications, one per line. Its stem names the
    /// profile.
    pub applications: Option<&'a Path>,
    /// File listing the native application shortcuts, one per line.
    pub shortcuts: Option<&'a Path>,
    /// Directory holding `<shortcut>.json` for each listed shortcut.
    pub shortcuts_directory: Option<&'a Path>,
    /// Directory whose `*.json` files are seeded as package repositories.
    pub repositories_directory: Option<&'a Path>,
}

impl Profile {
    /// Parse a list file: one name per line, blank lines ignored.
    pub fn parse_list(text: &str) -> Result<Vec<String>> {
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|name| {
                validate_name(name)?;
                Ok(name.to_owned())
            })
            .collect()
    }

    /// Load a profile from list files and directories.
    pub fn load(sources: &ProfileSources<'_>) -> Result<Self> {
        let mut profile = Self::default();
        if let Some(applications) = sources.applications {
            profile.name = applications
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or("the applications list must have a UTF-8 file name")?
                .to_owned();
            profile.applications = Self::parse_list(&read_text(applications)?)?;
        }
        match (sources.shortcuts, sources.shortcuts_directory) {
            (Some(shortcuts), Some(directory)) => {
                for name in Self::parse_list(&read_text(shortcuts)?)? {
                    let name = format!("{name}.json");
                    let path = directory.join(&name);
                    let content = fs::read(&path)
                        .map_err(|error| format!("unknown shortcut {}: {error}", path.display()))?;
                    profile.shortcuts.push(ProfileFile { name, content });
                }
            }
            (None, None) => {}
            _ => return Err("shortcuts require both a list and a shortcuts directory".into()),
        }
        if let Some(directory) = sources.repositories_directory {
            profile.repositories = read_json_files(directory)?;
        }
        Ok(profile)
    }
}

fn read_text(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()).into())
}

/// Read every `*.json` regular file of `directory`, in name order.
fn read_json_files(directory: &Path) -> Result<Vec<ProfileFile>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "file names must be UTF-8")?;
        if !name.ends_with(".json") {
            continue;
        }
        if !entry.file_type()?.is_file() {
            return Err(format!("not a regular file: {}", entry.path().display()).into());
        }
        files.push(ProfileFile {
            content: fs::read(entry.path())?,
            name,
        });
    }
    files.sort_by(|first, second| first.name.cmp(&second.name));
    Ok(files)
}

/// Reject names that are empty, special or that would leave their directory.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.contains(['/', '\\', '\0'])
        || name.len() > 255
    {
        return Err(format!("invalid name: {name:?}").into());
    }
    Ok(())
}

/// Package identifier of an official guest application, e.g. `xila.pixel-painter`.
pub fn package_identifier(name: &str) -> String {
    format!("xila.{}", name.replace('_', "-"))
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A temporary staging directory, removed when dropped.
#[derive(Debug)]
pub struct Staging(PathBuf);

impl Staging {
    fn create() -> Result<Self> {
        loop {
            let path = std::env::temp_dir().join(format!(
                "xila-bundle-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Root of the staged image contents.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Copy a verified bundle directory, rejecting anything but regular files and
/// directories. Entries are visited in sorted order for reproducible images.
fn copy_bundle(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination)?;
    let mut entries = fs::read_dir(source)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    for path in entries {
        let metadata = fs::symlink_metadata(&path)?;
        let target = destination.join(path.file_name().ok_or("invalid bundle entry")?);
        if metadata.is_dir() {
            copy_bundle(&path, &target)?;
        } else if metadata.is_file() {
            fs::copy(&path, &target)?;
        } else {
            return Err(format!("bundle entry is not a regular file: {}", path.display()).into());
        }
    }
    Ok(())
}

/// Stage `profile` as an image source directory, from verified bundle
/// directories named by package identifier
/// (`<bundles>/xila.clock/.xila/manifest.json`, ...), as produced by Platforms'
/// `xila-prepare-applications` or `xila package bundle build`.
pub fn stage(profile: &Profile, bundles_directory: &Path) -> Result<Staging> {
    let staging = Staging::create()?;
    for directory in DIRECTORIES {
        fs::create_dir_all(staging.0.join(directory))?;
    }
    fs::write(
        staging.0.join(&authentication::SEED_PENDING_PATH[1..]),
        authentication::SEED_PENDING_CONTENT,
    )?;
    for name in &profile.applications {
        validate_name(name)?;
        let identifier = package_identifier(name);
        let source = bundles_directory.join(&identifier);
        if !fs::symlink_metadata(source.join(".xila/manifest.json"))
            .is_ok_and(|metadata| metadata.is_file())
        {
            return Err(format!(
                "missing bundle {identifier} in {}",
                bundles_directory.display()
            )
            .into());
        }
        copy_bundle(
            &source,
            &staging.0.join(BUNDLES_DIRECTORY).join(&identifier),
        )?;
    }
    for (directory, files) in [
        (SHORTCUTS_DIRECTORY, &profile.shortcuts),
        (REPOSITORIES_DIRECTORY, &profile.repositories),
    ] {
        for file in files {
            validate_name(&file.name)?;
            let path = staging.0.join(directory).join(&file.name);
            if path.exists() {
                return Err(format!("duplicate profile file: {}", file.name).into());
            }
            fs::write(path, &file.content)?;
        }
    }
    Ok(staging)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "xila-tools-profile-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn package_identifiers_use_dashes() {
        assert_eq!(package_identifier("pixel_painter"), "xila.pixel-painter");
        assert_eq!(package_identifier("clock"), "xila.clock");
    }

    #[test]
    fn lists_ignore_blank_lines_and_reject_paths() {
        assert_eq!(
            Profile::parse_list("clock\n\n  digits \n").unwrap(),
            ["clock", "digits"]
        );
        for invalid in ["../escape", "a/b", "..", "a\\b"] {
            assert!(Profile::parse_list(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn loads_lists_shortcuts_and_repositories() {
        let root = temporary("load");
        fs::write(root.join("native.txt"), "clock\ndigits\n").unwrap();
        fs::write(root.join("native-shortcuts.txt"), "terminal\n").unwrap();
        fs::create_dir(root.join("shortcuts")).unwrap();
        fs::write(root.join("shortcuts/terminal.json"), b"{}").unwrap();
        fs::create_dir(root.join("repositories")).unwrap();
        fs::write(root.join("repositories/xila.json"), b"{\"name\":\"xila\"}").unwrap();
        fs::write(root.join("repositories/README.md"), b"ignored").unwrap();
        let profile = Profile::load(&ProfileSources {
            applications: Some(&root.join("native.txt")),
            shortcuts: Some(&root.join("native-shortcuts.txt")),
            shortcuts_directory: Some(&root.join("shortcuts")),
            repositories_directory: Some(&root.join("repositories")),
        })
        .unwrap();
        assert_eq!(profile.name, "native");
        assert_eq!(profile.applications, ["clock", "digits"]);
        assert_eq!(
            profile.shortcuts,
            [ProfileFile {
                name: "terminal.json".into(),
                content: b"{}".to_vec()
            }]
        );
        assert_eq!(profile.repositories.len(), 1);
        assert_eq!(profile.repositories[0].name, "xila.json");

        let missing = Profile::load(&ProfileSources {
            shortcuts: Some(&root.join("native.txt")),
            shortcuts_directory: Some(&root.join("shortcuts")),
            ..ProfileSources::default()
        })
        .unwrap_err();
        assert!(missing.to_string().contains("unknown shortcut"));
        assert!(
            Profile::load(&ProfileSources {
                shortcuts: Some(&root.join("native-shortcuts.txt")),
                ..ProfileSources::default()
            })
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stages_the_layout_bundles_and_profile_files() {
        let root = temporary("stage");
        let bundle = root.join("bundles/xila.pixel-painter");
        fs::create_dir_all(bundle.join(".xila")).unwrap();
        fs::write(bundle.join(".xila/manifest.json"), b"manifest").unwrap();
        let profile = Profile {
            name: "test".into(),
            applications: vec!["pixel_painter".into()],
            shortcuts: vec![ProfileFile {
                name: "terminal.json".into(),
                content: b"shortcut".to_vec(),
            }],
            repositories: vec![ProfileFile {
                name: "xila.json".into(),
                content: b"repository".to_vec(),
            }],
        };
        let staging = stage(&profile, &root.join("bundles")).unwrap();
        let path = staging.path().to_owned();
        for directory in DIRECTORIES {
            assert!(path.join(directory).is_dir(), "{directory}");
        }
        assert_eq!(
            fs::read(path.join("applications/bundles/xila.pixel-painter/.xila/manifest.json"))
                .unwrap(),
            b"manifest"
        );
        assert_eq!(
            fs::read(path.join(SHORTCUTS_DIRECTORY).join("terminal.json")).unwrap(),
            b"shortcut"
        );
        assert_eq!(
            fs::read(path.join(REPOSITORIES_DIRECTORY).join("xila.json")).unwrap(),
            b"repository"
        );
        assert!(path.join("system/accounts/.seed-pending").is_file());
        drop(staging);
        assert!(!path.exists());

        let mut duplicate = profile.clone();
        duplicate.shortcuts.push(duplicate.shortcuts[0].clone());
        assert!(stage(&duplicate, &root.join("bundles")).is_err());
        let mut missing = profile;
        missing.applications.push("clock".into());
        let error = stage(&missing, &root.join("bundles")).unwrap_err();
        assert!(error.to_string().contains("missing bundle xila.clock"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bundles_with_symbolic_links_are_rejected() {
        let root = temporary("symbolic-link");
        let bundle = root.join("bundles/xila.clock");
        fs::create_dir_all(bundle.join(".xila")).unwrap();
        fs::write(bundle.join(".xila/manifest.json"), b"manifest").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", bundle.join("link")).unwrap();
        let profile = Profile {
            applications: vec!["clock".into()],
            ..Profile::default()
        };
        assert!(stage(&profile, &root.join("bundles")).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
