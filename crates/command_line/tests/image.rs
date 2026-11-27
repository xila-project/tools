//! End-to-end tests of `xila image build`, ported from Platforms' `fs_image`.
#![cfg(any(target_os = "linux", target_os = "macos"))]
use image::package_identifier;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

const NATIVE: &str = "calculator\nclock\ndigits\npixel_painter\nsynth\ntext_editor\nweather\n";
const BROWSER: &str = "calculator\nclock\ndigits\npixel_painter\nsynth\ntext_editor\n";
const REPOSITORY: &str = r#"{"name":"xila","url":"https://registry.xila.dev","root_key":"AJEDFQ2MPtUO0vu/BnC7xX5Ag8sBuq6DF5+ShKfCQgU="}"#;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "xila-tools-seed-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("root/applications/executables")).unwrap();
        fs::write(
            path.join("root/applications/executables/example"),
            vec![42; 18000],
        )
        .unwrap();
        // A profile laid out as in Platforms: lists, shortcuts and repositories.
        let profiles = path.join("profile/profiles");
        fs::create_dir_all(&profiles).unwrap();
        fs::write(profiles.join("native.txt"), NATIVE).unwrap();
        fs::write(profiles.join("browser.txt"), BROWSER).unwrap();
        fs::write(
            profiles.join("native-shortcuts.txt"),
            "activity\nterminal\n",
        )
        .unwrap();
        fs::create_dir_all(path.join("profile/shortcuts")).unwrap();
        for name in ["activity", "terminal"] {
            fs::write(
                path.join(format!("profile/shortcuts/{name}.json")),
                format!(r#"{{"entrypoint":"/applications/executables/{name}"}}"#),
            )
            .unwrap();
        }
        fs::create_dir_all(path.join("profile/repositories")).unwrap();
        fs::write(path.join("profile/repositories/xila.json"), REPOSITORY).unwrap();
        Self(path)
    }

    fn xila(&self) -> Command {
        Command::new(env!("CARGO_BIN_EXE_xila"))
    }

    fn build(&self, name: &str) -> Output {
        self.xila()
            .args(["image", "build"])
            .arg(self.0.join(name))
            .args(["4194304", "--root"])
            .arg(self.0.join("root"))
            .output()
            .unwrap()
    }

    fn build_profile(&self, name: &str, profile: &str, bundles: &Path) -> Output {
        self.build_profile_with(name, profile, bundles, &["4194304"])
    }

    fn build_profile_with(
        &self,
        name: &str,
        profile: &str,
        bundles: &Path,
        geometry: &[&str],
    ) -> Output {
        let data = self.0.join("profile");
        self.xila()
            .args(["image", "build"])
            .arg(self.0.join(name))
            .args(geometry)
            .arg("--profile")
            .arg(data.join(format!("profiles/{profile}.txt")))
            .arg("--bundles-directory")
            .arg(bundles)
            .arg("--shortcuts")
            .arg(data.join("profiles/native-shortcuts.txt"))
            .arg("--shortcuts-directory")
            .arg(data.join("shortcuts"))
            .arg("--repositories-directory")
            .arg(data.join("repositories"))
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_bundle(root: &Path, application: &str) {
    let bundle = root.join(package_identifier(application));
    fs::create_dir_all(bundle.join(".xila")).unwrap();
    fs::create_dir_all(bundle.join("executables")).unwrap();
    fs::write(
        bundle.join(".xila/manifest.json"),
        format!("manifest for {application}"),
    )
    .unwrap();
    fs::write(
        bundle.join(format!("executables/{application}.wasm")),
        format!("guest fixture for {application}"),
    )
    .unwrap();
    fs::write(
        bundle.join("shortcut.json"),
        format!("shortcut for {application}"),
    )
    .unwrap();
}

#[test]
fn populated_images_are_verified_and_reproducible() {
    let fixture = Fixture::new();
    for name in ["one.img", "two.img"] {
        let output = fixture.build(name);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("Verified"));
    }
    assert_eq!(
        fs::read(fixture.0.join("one.img")).unwrap(),
        fs::read(fixture.0.join("two.img")).unwrap()
    );
    let before = fs::read(fixture.0.join("one.img")).unwrap();
    assert!(!fixture.build("one.img").status.success());
    assert_eq!(before, fs::read(fixture.0.join("one.img")).unwrap());
}

#[test]
fn rejects_symbolic_links_without_leaving_an_image() {
    let fixture = Fixture::new();
    std::os::unix::fs::symlink("/etc/passwd", fixture.0.join("root/link")).unwrap();
    assert!(!fixture.build("bad.img").status.success());
    assert!(!fixture.0.join("bad.img").exists());
}

#[test]
fn out_of_space_removes_incomplete_output() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.join("root/applications/executables/huge"),
        vec![1; 5 * 1024 * 1024],
    )
    .unwrap();
    assert!(!fixture.build("full.img").status.success());
    assert!(!fixture.0.join("full.img").exists());
}

#[test]
fn browser_and_native_profiles_contain_their_declared_applications() {
    let fixture = Fixture::new();
    let bundles = fixture.0.join("bundles");
    for application in [
        "calculator",
        "weather",
        "clock",
        "digits",
        "pixel_painter",
        "synth",
        "text_editor",
    ] {
        write_bundle(&bundles, application);
    }
    for profile in ["native", "browser"] {
        let output = fixture.build_profile(&format!("{profile}.img"), profile, &bundles);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains(&format!(
            "Prepared {profile} bundle: calculator,clock,digits,pixel_painter,synth,text_editor"
        )));
        let image = fs::read(fixture.0.join(format!("{profile}.img"))).unwrap();
        let contains = |content: &[u8]| image.windows(content.len()).any(|bytes| bytes == content);
        for application in ["digits", "synth", "text_editor"] {
            for content in [
                format!("guest fixture for {application}"),
                format!("shortcut for {application}"),
                format!("manifest for {application}"),
            ] {
                assert!(contains(content.as_bytes()), "{profile}: {content}");
            }
        }
        assert_eq!(text.contains(",weather"), profile == "native");
        assert_eq!(contains(b"guest fixture for weather"), profile == "native");
        assert!(contains(b"/applications/executables/activity"));
        assert!(contains(b"/applications/executables/terminal"));
        assert!(contains(REPOSITORY.as_bytes()));
    }
}

#[test]
fn missing_bundle_is_an_error_without_output() {
    let fixture = Fixture::new();
    let bundles = fixture.0.join("bundles");
    for application in ["calculator", "clock", "digits", "pixel_painter", "synth"] {
        write_bundle(&bundles, application);
    }
    let output = fixture.build_profile("missing.img", "browser", &bundles);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("missing bundle dev.xila.text-editor")
    );
    assert!(!fixture.0.join("missing.img").exists());
}

#[test]
fn optional_capacity_defaults_when_population_flags_follow_output() {
    let fixture = Fixture::new();
    let output = fixture
        .xila()
        .args(["image", "build"])
        .arg(fixture.0.join("default.img"))
        .arg("--root")
        .arg(fixture.0.join("root"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::metadata(fixture.0.join("default.img")).unwrap().len(),
        16 * 1024 * 1024
    );
}

#[test]
fn unsupported_entries_and_non_utf8_names_are_rejected() {
    use std::os::unix::{ffi::OsStringExt, fs::PermissionsExt};
    let fixture = Fixture::new();
    // A FIFO is a special file like a socket, but its path has no length
    // limit, so the test works under any temporary directory.
    let fifo = fixture.0.join("root/fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: `name` is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(!fixture.build("fifo.img").status.success());
    assert!(!fixture.0.join("fifo.img").exists());
    fs::remove_file(fifo).unwrap();
    let invalid = fixture
        .0
        .join("root")
        .join(std::ffi::OsString::from_vec(vec![0xff]));
    fs::write(&invalid, b"invalid path").unwrap();
    assert!(!fixture.build("utf8.img").status.success());
    assert!(!fixture.0.join("utf8.img").exists());
    fs::remove_file(invalid).unwrap();
    assert!(fixture.build("before.img").status.success());
    fs::set_permissions(
        fixture.0.join("root/applications/executables/example"),
        fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    fs::File::open(fixture.0.join("root/applications/executables/example"))
        .unwrap()
        .set_modified(std::time::SystemTime::UNIX_EPOCH)
        .unwrap();
    assert!(fixture.build("after.img").status.success());
    assert_eq!(
        fs::read(fixture.0.join("before.img")).unwrap(),
        fs::read(fixture.0.join("after.img")).unwrap()
    );
}

#[test]
fn both_profiles_require_the_digits_bundle() {
    let fixture = Fixture::new();
    let bundles = fixture.0.join("bundles");
    for application in [
        "calculator",
        "clock",
        "pixel_painter",
        "synth",
        "text_editor",
        "weather",
    ] {
        write_bundle(&bundles, application);
    }
    for profile in ["native", "browser"] {
        let image = format!("missing-digits-{profile}.img");
        let output = fixture.build_profile(&image, profile, &bundles);
        assert!(
            !output.status.success(),
            "{profile} silently omitted Digits"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("missing bundle dev.xila.digits"));
        assert!(!fixture.0.join(image).exists());
    }
}

#[test]
fn invalid_command_lines_fail_without_output() {
    let fixture = Fixture::new();
    let output = fixture
        .xila()
        .args(["image", "build"])
        .arg(fixture.0.join("invalid.img"))
        .args(["--profile", "native.txt"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("xila: "));
    assert!(!fixture.0.join("invalid.img").exists());
}

/// Size of the WT32-SC01 Plus `storage` partition: 3312 blocks of 4096 bytes.
const FLASH_PARTITION_SIZE: &str = "13565952";

#[test]
fn raw_flash_profiles_are_reproducible_and_have_no_master_boot_record() {
    let fixture = Fixture::new();
    let bundles = fixture.0.join("bundles");
    for application in [
        "calculator",
        "clock",
        "digits",
        "pixel_painter",
        "synth",
        "text_editor",
    ] {
        write_bundle(&bundles, application);
    }
    let geometry = [
        FLASH_PARTITION_SIZE,
        "--layout",
        "raw",
        "--block-size",
        "4096",
    ];
    for name in ["flash-one.img", "flash-two.img"] {
        let output = fixture.build_profile_with(name, "browser", &bundles, &geometry);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("Verified raw LittleFS contents and metadata"),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    let image = fs::read(fixture.0.join("flash-one.img")).unwrap();
    assert_eq!(image, fs::read(fixture.0.join("flash-two.img")).unwrap());
    assert_eq!(image.len().to_string(), FLASH_PARTITION_SIZE);
    assert!(image[8..16] == *b"littlefs" || image[4096 + 8..4096 + 16] == *b"littlefs");
    assert_ne!(image[510..512], [0x55, 0xAA]);
    let contains = |content: &[u8]| image.windows(content.len()).any(|bytes| bytes == content);
    assert!(contains(b"guest fixture for synth"));
    assert!(contains(b"/applications/executables/terminal"));
    assert!(contains(REPOSITORY.as_bytes()));
}

#[test]
fn invalid_geometries_create_no_image() {
    let fixture = Fixture::new();
    for geometry in [
        &["4194304", "--block-size", "4096"][..],
        &["4194304", "--layout", "raw", "--block-size", "3000"],
        &["4194560", "--layout", "raw", "--block-size", "4096"],
    ] {
        let output = fixture
            .xila()
            .args(["image", "build"])
            .arg(fixture.0.join("invalid.img"))
            .args(geometry)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{geometry:?}");
        assert!(!fixture.0.join("invalid.img").exists());
    }
}
