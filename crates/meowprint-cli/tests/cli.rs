//! Offline CLI behavior.

use std::process::Command;

fn miao() -> Command {
    Command::new(env!("CARGO_BIN_EXE_meowprint"))
}

#[test]
fn previews_work_without_bluetooth_and_save_exact_pixels() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("source.png");
    image::GrayImage::from_pixel(64, 32, image::Luma([0])).save(&source)?;
    for (name, args) in [
        ("image", vec!["image"]),
        ("qr", vec!["qr", "miao-cli-test"]),
    ] {
        let output = directory.path().join(format!("{name}.png"));
        let mut command = miao();
        command
            .args(["preview", "--driver", "gt01", "--output"])
            .arg(&output)
            .args(args);
        if name == "image" {
            command.arg(&source);
        }
        let result = command.output()?;
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let preview = image::open(&output)?.to_luma8();
        let reference = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(format!("{name}.png"));
        assert_eq!(
            preview,
            image::open(reference)?.to_luma8(),
            "{name} pixels changed"
        );
        assert_eq!(preview.width(), 384);
        assert!(preview.pixels().all(|p| p[0] == 0 || p[0] == 255));
        if name == "image" {
            assert_eq!(preview.height(), 192);
        }
        if name == "qr" {
            let mut prepared = rqrr::PreparedImage::prepare(preview);
            let grids = prepared.detect_grids();
            assert_eq!(grids[0].decode()?.1, "miao-cli-test");
        }
    }
    Ok(())
}

#[test]
fn text_preview_uses_the_embedded_font() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let font = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/fonts/Roboto.ttf");
    let mut previews = Vec::new();
    for name in ["default", "explicit"] {
        let output = directory.path().join(format!("{name}.png"));
        let mut command = miao();
        command
            .current_dir(directory.path())
            .args(["preview", "--driver", "gt01", "--output"])
            .arg(&output)
            .args(["text", "Hello! Привет! Γειά! café € £ 0123456789 Il1 O0"]);
        if name == "explicit" {
            command.arg("--font").arg(&font);
        }
        let result = command.output()?;
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let preview = image::open(&output)?.to_luma8();
        assert_eq!(preview.width(), 384);
        assert!(preview.pixels().any(|p| p[0] == 0));
        assert!(preview.pixels().all(|p| p[0] == 0 || p[0] == 255));
        previews.push(preview);
    }
    assert_eq!(previews[0], previews[1]);
    Ok(())
}

#[test]
fn missing_font_fails_before_bluetooth_discovery() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let font = directory.path().join("missing.ttf");
    let result = miao()
        .args(["print", "--device", "not-a-printer", "--driver", "gt01"])
        .args(["text", "Test text", "--font"])
        .arg(&font)
        .output()?;
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains("Cannot read font"), "{error}");
    assert!(error.contains(&font.display().to_string()), "{error}");
    assert!(!error.contains("Finding printer"), "{error}");
    Ok(())
}
#[test]
fn invalid_print_preview_path_fails_before_bluetooth_discovery()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let parent = directory.path().join("not-a-directory");
    std::fs::write(&parent, [])?;
    let output = miao()
        .args([
            "print",
            "--device",
            "not-a-printer",
            "--driver",
            "gt01",
            "--preview",
        ])
        .arg(parent.join("preview.png"))
        .args(["text", "Preview must be saved before discovery"])
        .output()?;
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("Cannot save the preview. No print data was submitted."),
        "{error}"
    );
    assert!(!error.contains("Finding printer"), "{error}");
    Ok(())
}

#[test]
fn grayscale_preview_preserves_the_reference_levels() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("gray.png");
    let output = directory.path().join("preview.png");
    image::GrayImage::from_fn(384, 2, |x, _| {
        image::Luma([u8::try_from(x % 256).unwrap_or_default()])
    })
    .save(&source)?;
    let result = miao()
        .args(["preview", "--driver", "x6h", "--output"])
        .arg(&output)
        .arg("image")
        .arg(&source)
        .arg("--grayscale")
        .output()?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let reference =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gray.png");
    assert_eq!(
        image::open(output)?.to_luma8(),
        image::open(reference)?.to_luma8()
    );
    Ok(())
}
#[test]
fn unsupported_configuration_fails_before_bluetooth_discovery()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let missing = directory.path().join("missing.png");
    let unused = directory.path().join("unused.png");
    for (flags, expected) in [
        (["--driver", "mxw01", "--feed", "1"], "not supported"),
        (["--driver", "gt01", "--intensity", "1"], "not supported"),
        (
            ["--driver", "gt01", "--speed", "0"],
            "Speed must be from 4 through 255",
        ),
        (
            ["--driver", "gt01", "--feed-speed", "3"],
            "Speed must be from 4 through 255",
        ),
        (
            ["--driver", "gt01", "--quality", "6"],
            "Quality must be from 1 through 5",
        ),
        (
            ["--driver", "x6h", "--density", "201"],
            "Density must be from 0 through 200",
        ),
        (
            ["--driver", "v5g", "--density", "0"],
            "Density must be from 1 through 200",
        ),
    ] {
        let result = miao()
            .args(["print", "--device", "not-a-printer"])
            .args(flags)
            .args(["qr", "test"])
            .output()?;
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(!error.contains("Finding printer"), "{error}");
        assert!(error.contains(expected), "{error}");
    }
    for command in ["print", "preview"] {
        let mut cli = miao();
        cli.args([command, "--driver", "gt01"]);
        if command == "print" {
            cli.args(["--device", "not-a-printer"]);
        } else {
            cli.arg("--output").arg(&unused);
        }
        let result = cli.arg("image").arg(&missing).arg("--grayscale").output()?;
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(
            error.contains("does not support four-bit grayscale"),
            "{error}"
        );
        assert!(!error.contains("Cannot open"), "{error}");
        assert!(!error.contains("Finding printer"), "{error}");
    }
    Ok(())
}

#[test]
fn colors_and_transparency_are_converted_before_resizing() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let color_path = directory.path().join("color.png");
    let gray_path = directory.path().join("gray.png");
    let colors = [
        [0, 0, 0, 0],
        [255, 0, 0, 255],
        [0, 255, 0, 255],
        [0, 0, 255, 255],
        [0, 0, 0, 128],
        [0, 12, 0, 255],
    ];
    // The last color exercises image-rs luminance rounding at a Gray4 boundary.
    let levels = [255, 54, 182, 18, 127, 8];
    image::RgbaImage::from_fn(6, 1, |x, _| image::Rgba(colors[x as usize])).save(&color_path)?;
    image::GrayImage::from_fn(6, 1, |x, _| image::Luma([levels[x as usize]])).save(&gray_path)?;
    for flags in [
        vec!["--dither", "ostromoukhov"],
        vec!["--dither", "stucki"],
        vec!["--dither", "floyd-steinberg"],
        vec!["--dither", "threshold"],
        vec!["--grayscale"],
    ] {
        let mut previews = Vec::new();
        for source in [&color_path, &gray_path] {
            let output = directory.path().join("preview.png");
            let result = miao()
                .args(["preview", "--driver", "x6h", "--output"])
                .arg(&output)
                .arg("image")
                .arg(source)
                .args(&flags)
                .output()?;
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            previews.push(image::open(output)?.to_luma8());
        }
        assert_eq!(previews[0], previews[1], "{flags:?}");
    }
    Ok(())
}

#[test]
fn oversized_image_is_rejected_before_decoding_and_bluetooth_discovery()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("oversized.bmp");
    let output = directory.path().join("preview.png");
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(1, 1).write_to(&mut encoded, image::ImageFormat::Bmp)?;
    let mut bytes = encoded.into_inner();
    // Declare a 300 MB RGB raster within the dimension limits, without its pixel data.
    // It must fail the allocation check before decoding reaches the truncated data.
    bytes[18..22].copy_from_slice(&10_000_i32.to_le_bytes());
    bytes[22..26].copy_from_slice(&10_000_i32.to_le_bytes());
    std::fs::write(&source, bytes)?;
    for command in ["preview", "print"] {
        let mut cli = miao();
        cli.args([command, "--driver", "gt01"]);
        if command == "preview" {
            cli.arg("--output").arg(&output);
        } else {
            cli.args(["--device", "not-a-printer"]);
        }
        let result = cli.arg("image").arg(&source).output()?;
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(error.contains("Memory limit exceeded"), "{error}");
        assert!(!error.contains("Finding printer"), "{error}");
        assert!(!output.exists());
    }
    Ok(())
}
