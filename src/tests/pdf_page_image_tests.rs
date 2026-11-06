//! `read` on a scanned PDF hands the page images to a vision-capable model.

use rig::tool::Tool;

use crate::agent::tools::pdf_text::test_pdf::{Content, build_with, first_extra_object};
use crate::agent::tools::{ReadArgs, ReadTool, ReadTracker, read_image::ImageReadSupport};

const JPEG: &[u8] = include_bytes!("../agent/tools/fixtures/read-image64/3x2.jpg");

fn scanned_pdf(pages: usize) -> Vec<u8> {
    let mut image = format!(
        "<< /Type /XObject /Subtype /Image /Width 3 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
        JPEG.len()
    )
    .into_bytes();
    image.extend_from_slice(JPEG);
    image.extend_from_slice(b"\nendstream");
    let contents: Vec<Content> = (0..pages)
        .map(|_| Content::Plain("q 612 0 0 792 0 0 cm /Im0 Do Q".into()))
        .collect();
    build_with(
        &contents,
        false,
        &format!("/XObject << /Im0 {} 0 R >>", first_extra_object(pages)),
        vec![image],
    )
}

fn tool(root: &std::path::Path, vision: bool) -> ReadTool {
    let mut tracker = ReadTracker::new(true);
    if vision {
        tracker.image_support = ImageReadSupport::captured("anthropic", "claude-fable-5-1", false);
    }
    ReadTool::new_with_tracker(None, None, Some(16), 100, tracker).with_workspace(root)
}

fn args(pages: Option<&str>) -> ReadArgs {
    ReadArgs {
        path: "scan.pdf".into(),
        pages: pages.map(str::to_owned),
        ..ReadArgs::default()
    }
}

fn envelope(output: &str) -> (String, Vec<serde_json::Value>) {
    let value: serde_json::Value = serde_json::from_str(output)
        .unwrap_or_else(|error| panic!("not an image envelope ({error}): {output}"));
    (
        value["response"].as_str().unwrap().to_owned(),
        value["parts"].as_array().unwrap().clone(),
    )
}

#[tokio::test]
async fn scanned_pages_come_back_as_image_blocks_for_a_vision_model() {
    let root = super::OwnedTestDirectory::new("pdf-page-images");
    std::fs::write(root.path().join("scan.pdf"), scanned_pdf(2)).unwrap();
    let output = tool(root.path(), true)
        .call(args(Some("1-2")))
        .await
        .unwrap();
    let (text, parts) = envelope(&output);
    assert_eq!(parts.len(), 2, "{output}");
    assert!(parts.iter().all(|part| part["type"] == "image"));
    assert!(
        text.contains("no text could be extracted from page(s) 1-2"),
        "{text}"
    );
    assert!(
        text.contains("image 1 = page 1 (3 x 2, image/jpeg), image 2 = page 2"),
        "{text}"
    );
    assert!(!text.contains("pdftoppm"), "{text}");
}

#[tokio::test]
async fn page_images_are_capped_per_read_with_a_way_to_continue() {
    let root = super::OwnedTestDirectory::new("pdf-page-images-cap");
    std::fs::write(root.path().join("scan.pdf"), scanned_pdf(6)).unwrap();
    let output = tool(root.path(), true).call(args(None)).await.unwrap();
    let (text, parts) = envelope(&output);
    assert_eq!(parts.len(), 4, "{text}");
    assert!(text.contains("image 4 = page 4"), "{text}");
    assert!(
        text.contains("page(s) 5-6 (limit of 4 images per read"),
        "{text}"
    );
    assert!(text.contains("continue with pages:\"5-6\""), "{text}");
    let rest = tool(root.path(), true)
        .call(args(Some("5-6")))
        .await
        .unwrap();
    assert_eq!(envelope(&rest).1.len(), 2);
}

#[tokio::test]
async fn without_a_vision_model_scanned_pdfs_keep_the_text_only_answer() {
    let root = super::OwnedTestDirectory::new("pdf-page-images-closed");
    std::fs::write(root.path().join("scan.pdf"), scanned_pdf(1)).unwrap();
    // No captured image-capable transport: default closed, no image blocks.
    let output = tool(root.path(), false).call(args(None)).await.unwrap();
    assert!(
        serde_json::from_str::<serde_json::Value>(&output).is_err(),
        "{output}"
    );
    assert!(output.contains("probably scanned images"), "{output}");
    assert!(output.contains("pdftoppm"), "{output}");
    // A model declared text-only is also refused images.
    let mut tracker = ReadTracker::new(true);
    tracker.image_support = ImageReadSupport::captured("openai", "gpt-3.5-turbo", false);
    let text_only = ReadTool::new_with_tracker(None, None, Some(16), 100, tracker)
        .with_workspace(root.path())
        .call(args(None))
        .await
        .unwrap();
    assert!(text_only.contains("pdftoppm"), "{text_only}");
}

#[tokio::test]
async fn pages_with_a_text_layer_never_get_images() {
    let root = super::OwnedTestDirectory::new("pdf-page-images-text");
    let pdf = {
        let mut image = format!(
            "<< /Type /XObject /Subtype /Image /Width 3 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
            JPEG.len()
        )
        .into_bytes();
        image.extend_from_slice(JPEG);
        image.extend_from_slice(b"\nendstream");
        build_with(
            &[
                Content::Plain("BT /F1 12 Tf 72 720 Td (Real text) Tj ET q /Im0 Do Q".into()),
                Content::Plain("q 612 0 0 792 0 0 cm /Im0 Do Q".into()),
            ],
            false,
            &format!("/XObject << /Im0 {} 0 R >>", first_extra_object(2)),
            vec![image],
        )
    };
    std::fs::write(root.path().join("scan.pdf"), pdf).unwrap();
    let output = tool(root.path(), true).call(args(None)).await.unwrap();
    let (text, parts) = envelope(&output);
    assert_eq!(
        parts.len(),
        1,
        "only the scanned page 2 is attached: {text}"
    );
    assert!(text.contains("Real text"), "{text}");
    assert!(text.contains("image 1 = page 2"), "{text}");
    assert!(!text.contains("page 1 ("), "{text}");
}
