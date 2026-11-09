use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use lopdf::{Document, Object, Stream, dictionary};

fn unique_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("pdf-translate-test-{}-{nanos}", std::process::id()))
}

fn write_sample_pdf(path: &std::path::Path) {
    let mut document = Document::with_version("1.5");
    let pages_id = document.new_object_id();
    let font_id = document.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
    });
    let contents_id = document.add_object(Stream::new(
        dictionary! {},
        b"BT /F1 18 Tf 72 720 Td (Hello world) Tj ET".to_vec(),
    ));
    let page_id = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => contents_id,
        "Resources" => dictionary! {
            "Font" => dictionary! {
                "F1" => Object::Reference(font_id),
            },
        },
    });
    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    document.save(path).unwrap();
}

#[test]
fn inspect_extracts_text_from_simple_pdf() {
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let pdf = dir.join("sample.pdf");
    let json = dir.join("layout.json");
    write_sample_pdf(&pdf);

    let output = Command::new(env!("CARGO_BIN_EXE_pdf-translate"))
        .arg("inspect")
        .arg(&pdf)
        .arg("--pages")
        .arg("1")
        .arg("-o")
        .arg(&json)
        .output()
        .expect("failed to run pdf-translate");

    assert!(
        output.status.success(),
        "inspect failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
    let paragraphs = value["pages"][0]["paragraphs"].as_array().unwrap();
    let text = paragraphs
        .iter()
        .filter_map(|paragraph| paragraph["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let lines = value["pages"][0]["lines"].as_array().unwrap();
    for paragraph in paragraphs {
        assert!(paragraph.get("line_indices").is_none());
        for line in paragraph["lines"].as_array().unwrap() {
            assert!(lines.contains(line));
        }
    }
    assert!(text.contains("Hello world"), "unexpected text: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn conflicting_outputs_preserve_the_input() {
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("original.pdf");
    let alias = dir.join("alias");
    write_sample_pdf(&input);
    let original = std::fs::read(&input).unwrap();
    std::fs::hard_link(&input, &alias).unwrap();
    let output = dir.join("new.pdf");
    for (cmd, options) in [
        ("inspect", vec!["-o".as_ref(), alias.as_os_str()]),
        (
            "translate",
            vec![
                "--fake".as_ref(),
                "--dump-layout".as_ref(),
                input.as_os_str(),
            ],
        ),
        (
            "translate",
            vec![
                "--fake".as_ref(),
                "-o".as_ref(),
                output.as_os_str(),
                "--dump-layout".as_ref(),
                output.as_os_str(),
            ],
        ),
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_pdf-translate"))
            .arg(cmd)
            .arg(&input)
            .args(options)
            .env("XDG_CACHE_HOME", &dir)
            .output()
            .unwrap();
        assert!(!result.status.success(), "{cmd} accepted conflicting paths");
        assert_eq!(std::fs::read(&input).unwrap(), original);
        assert!(!output.exists());
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn explicit_config_file_is_loaded() {
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("sample.pdf");
    let config = dir.join("config.toml");
    write_sample_pdf(&input);
    std::fs::write(&config, "[typeset]\nmin_scale = 0.1\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pdf-translate"))
        .arg("translate")
        .arg(&input)
        .arg("--fake")
        .arg("--config")
        .arg(&config)
        .output()
        .expect("failed to run pdf-translate");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("min scale must be in"),
        "config was not applied: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_explicit_config_file_is_an_error() {
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("missing.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_pdf-translate"))
        .arg("inspect")
        .arg(dir.join("does-not-exist.pdf"))
        .arg("--config")
        .arg(&config)
        .output()
        .expect("failed to run pdf-translate");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot read configuration file"),
        "unexpected error: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn symlinked_input_and_output_directories_are_resolved_before_writing() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("input.pdf");
    write_sample_pdf(&input);
    let original = std::fs::read(&input).unwrap();
    let output = dir.path().join("new.pdf");
    let alias = dir.path().join("linked-dir");
    symlink(dir.path(), &alias).unwrap();
    let dangling = dir.path().join("dump-link");
    symlink("new.pdf", &dangling).unwrap();
    for dump in [alias.join("input.pdf"), alias.join("new.pdf"), dangling] {
        let result = Command::new(env!("CARGO_BIN_EXE_pdf-translate"))
            .arg("translate")
            .arg(&input)
            .arg("--fake")
            .arg("-o")
            .arg(&output)
            .arg("--dump-layout")
            .arg(dump)
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("distinct files"));
        assert_eq!(std::fs::read(&input).unwrap(), original);
        assert!(!output.exists());
    }
}
