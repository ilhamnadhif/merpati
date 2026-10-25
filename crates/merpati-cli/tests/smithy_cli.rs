//! `merpati import` of a Smithy model split over several files.

use std::path::{Path, PathBuf};
use std::process::Output;

const BIN: &str = env!("CARGO_BIN_EXE_merpati");

fn shop() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../merpati-convert/testdata/smithy-multi")
}

fn merpati(args: &[&str], cwd: &Path) -> Output {
    std::process::Command::new(BIN)
        .args(args)
        .current_dir(cwd)
        .env("MERPATI_NO_KEYCHAIN", "1")
        .env("NO_COLOR", "1")
        .output()
        .expect("run merpati")
}

fn text(o: &Output) -> String {
    format!("{}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn import_a_directory_or_several_files_as_one_model() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let ws = ws.to_str().unwrap();
    let shop = shop();

    let out = merpati(&["import", shop.to_str().unwrap(), "-w", ws], dir.path());
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("Detected Smithy (5 files)"), "{}", text(&out));
    assert!(text(&out).contains("Imported collection \"Shop Service\" (3 requests)"), "{}", text(&out));

    // Explicit files; names in warnings are the paths as given.
    let files: Vec<String> = ["main.smithy", "operations.smithy", "common.smithy", "traits.smithy", "item.json"]
        .iter()
        .map(|f| shop.join(f).to_string_lossy().into_owned())
        .collect();
    let mut args = vec!["import"];
    args.extend(files.iter().map(String::as_str));
    args.extend(["-w", ws, "--name", "Shop"]);
    let out = merpati(&args, dir.path());
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("Imported collection \"Shop\" (3 requests)"), "{}", text(&out));

    // Without the other files, the operations file alone has unknown targets
    // (warnings), but still imports.
    let out = merpati(&["import", &files[1], "-w", ws], dir.path());
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("unknown target `ItemId`"), "{}", text(&out));

    // A syntax error in one file fails the import and names the file.
    let broken = dir.path().join("broken.smithy");
    std::fs::write(&broken, "namespace example.shop\nstructure {").unwrap();
    let out = merpati(&["import", &files[0], broken.to_str().unwrap(), "-w", ws], dir.path());
    assert_ne!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("broken.smithy:2:11: error: expected a shape name"), "{}", text(&out));

    // Several files only make sense for Smithy.
    let other = dir.path().join("collection.json");
    std::fs::write(&other, "{\"info\": {\"name\": \"x\"}, \"item\": []}").unwrap();
    let out = merpati(&["import", &files[0], other.to_str().unwrap(), "-w", ws], dir.path());
    assert_ne!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("is not a Smithy model"), "{}", text(&out));
}

#[test]
fn import_uses_smithy_build_sources() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    let model = project.join("model");
    std::fs::create_dir_all(&model).unwrap();
    for f in ["main.smithy", "operations.smithy", "common.smithy", "traits.smithy", "item.json"] {
        std::fs::copy(shop().join(f), model.join(f)).unwrap();
    }
    // Not listed in smithy-build.json: must not be merged into the model.
    std::fs::write(project.join("scratch.smithy"), "namespace other\nstring Broken {").unwrap();
    std::fs::write(project.join("smithy-build.json"), r#"{"version": "1.0", "sources": ["model"]}"#).unwrap();
    let ws = dir.path().join("ws");
    let out = merpati(&["import", project.to_str().unwrap(), "-w", ws.to_str().unwrap()], dir.path());
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("Detected Smithy (5 files)"), "{}", text(&out));
    assert!(text(&out).contains("Imported collection \"Shop Service\" (3 requests)"), "{}", text(&out));

    std::fs::write(project.join("smithy-build.json"), r#"{"sources": "model"}"#).unwrap();
    let out = merpati(&["import", project.to_str().unwrap(), "-w", ws.to_str().unwrap()], dir.path());
    assert_ne!(out.status.code(), Some(0));
    assert!(text(&out).contains("must be an array"), "{}", text(&out));
}

/// CRC-32 (IEEE), for the test jar.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for b in data {
        crc ^= u32::from(*b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// A jar (zip archive) with uncompressed `(name, content)` entries.
fn jar(entries: &[(&str, &str)]) -> Vec<u8> {
    let (mut out, mut central) = (Vec::new(), Vec::new());
    for (name, content) in entries {
        let mut header = Vec::new();
        header.extend([0, 0, 0, 0, 0, 0]); // flags, method (stored), time
        header.extend([0, 0]); // date
        header.extend(crc32(content.as_bytes()).to_le_bytes());
        header.extend((content.len() as u32).to_le_bytes());
        header.extend((content.len() as u32).to_le_bytes());
        header.extend((name.len() as u16).to_le_bytes());
        header.extend([0, 0]); // extra length
        let offset = out.len() as u32;
        out.extend(0x0403_4b50u32.to_le_bytes());
        out.extend([20, 0]);
        out.extend(&header);
        out.extend(name.as_bytes());
        out.extend(content.as_bytes());
        central.extend(0x0201_4b50u32.to_le_bytes());
        central.extend([20, 0, 20, 0]);
        central.extend(&header);
        central.extend([0; 10]); // comment length, disk, attributes
        central.extend(offset.to_le_bytes());
        central.extend(name.as_bytes());
    }
    let (cd_offset, cd_size) = (out.len() as u32, central.len() as u32);
    out.extend(central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0; 4]);
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend(cd_size.to_le_bytes());
    out.extend(cd_offset.to_le_bytes());
    out.extend([0, 0]);
    out
}

/// `maven.dependencies` of smithy-build.json come from the local Maven
/// repository (`~/.m2/repository`, here under a temporary HOME); missing
/// ones are warnings.
#[test]
fn import_reads_maven_dependencies_from_the_local_repository() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join("model")).unwrap();
    std::fs::write(
        project.join("model/main.smithy"),
        r#"$version: "2"
namespace example.app

use example.shared#Greeting

service App {
    version: "1"
    operations: [Greet]
}

@http(method: "POST", uri: "/greet")
operation Greet {
    input := {
        greeting: Greeting
    }
}
"#,
    )
    .unwrap();
    std::fs::write(
        project.join("smithy-build.json"),
        r#"{"version": "1.0", "sources": ["model"],
            "maven": {"dependencies": ["com.example:shared:1.0.0", "com.example:missing:2.0.0"]}}"#,
    )
    .unwrap();
    let home = dir.path().join("home");
    let ws = dir.path().join("ws");
    let import = |home: &Path| {
        std::process::Command::new(BIN)
            .args(["import", project.to_str().unwrap(), "-w", ws.to_str().unwrap()])
            .current_dir(dir.path())
            .env("HOME", home)
            .env("MERPATI_NO_KEYCHAIN", "1")
            .output()
            .expect("run merpati")
    };

    // Without the jar the shared shape is unknown.
    let out = import(&home);
    let t = text(&out);
    assert_eq!(out.status.code(), Some(0), "{t}");
    assert!(t.contains("warning: Maven dependency com.example:shared:1.0.0 is not in the local repository"), "{t}");
    assert!(t.contains("unknown target `example.shared#Greeting`"), "{t}");

    let repo = home.join(".m2/repository/com/example/shared/1.0.0");
    std::fs::create_dir_all(&repo).unwrap();
    let shared =
        "$version: \"2\"\nnamespace example.shared\n\nstructure Greeting {\n    @required\n    text: String\n}\n";
    std::fs::write(
        repo.join("shared-1.0.0.jar"),
        jar(&[("META-INF/smithy/manifest", "shared.smithy\n"), ("META-INF/smithy/shared.smithy", shared)]),
    )
    .unwrap();
    let out = import(&home);
    let t = text(&out);
    assert_eq!(out.status.code(), Some(0), "{t}");
    // The project's file comes first and names the collection.
    assert!(t.contains("Detected Smithy (2 files)"), "{t}");
    assert!(t.contains("Imported collection \"App\""), "{t}");
    assert!(!t.contains("Greeting`"), "the dependency defines it: {t}");
    assert!(t.contains("com.example:missing:2.0.0 is not in the local repository"), "{t}");
    let out = merpati(&["export", "-w", ws.to_str().unwrap(), "-c", "App"], dir.path());
    assert!(String::from_utf8_lossy(&out.stdout).contains("text"), "{}", text(&out));
}
