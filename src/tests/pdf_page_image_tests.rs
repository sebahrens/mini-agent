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

fn native_tool(root: &std::path::Path, provider: &str, model: &str) -> ReadTool {
    let mut tracker = ReadTracker::new(true);
    tracker.image_support =
        ImageReadSupport::captured(provider, model, false).with_native_pdf_transport();
    ReadTool::new_with_tracker(None, None, Some(16), 100, tracker).with_workspace(root)
}

fn mixed_pdf() -> Vec<u8> {
    let mut image = format!(
        "<< /Type /XObject /Subtype /Image /Width 3 /Height 2 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {} >>\nstream\n",
        JPEG.len()
    )
    .into_bytes();
    image.extend_from_slice(JPEG);
    image.extend_from_slice(b"\nendstream");
    build_with(
        &[
            Content::Plain("BT /F1 12 Tf 72 720 Td (Real text) Tj ET".into()),
            Content::Plain("q 612 0 0 792 0 0 cm /Im0 Do Q".into()),
            // Vector-only figure: no text and no image XObject.
            Content::Plain("0 0 100 100 re f".into()),
        ],
        false,
        &format!("/XObject << /Im0 {} 0 R >>", first_extra_object(3)),
        vec![image],
    )
}

fn document_part(parts: &[serde_json::Value]) -> Vec<u8> {
    use base64::Engine;
    let documents: Vec<_> = parts.iter().filter(|p| p["type"] == "document").collect();
    assert_eq!(documents.len(), 1, "{parts:?}");
    assert_eq!(documents[0]["mimeType"], "application/pdf");
    base64::engine::general_purpose::STANDARD
        .decode(documents[0]["data"].as_str().unwrap())
        .unwrap()
}

#[tokio::test]
async fn native_transport_returns_one_pdf_block_with_only_the_textless_pages() {
    let root = super::OwnedTestDirectory::new("pdf-native-block");
    std::fs::write(root.path().join("mixed.pdf"), mixed_pdf()).unwrap();
    let output = native_tool(root.path(), "anthropic", "claude-fable-5-1")
        .call(ReadArgs {
            path: "mixed.pdf".into(),
            ..ReadArgs::default()
        })
        .await
        .unwrap();
    let (text, parts) = envelope(&output);
    assert_eq!(parts.len(), 1, "no page images beside the document: {text}");
    assert!(text.contains("Real text"), "{text}");
    assert!(
        text.contains("document page 1 = page 2, document page 2 = page 3"),
        "{text}"
    );
    assert!(!text.contains("pdftoppm"), "{text}");
    let pdf = document_part(&parts);
    assert!(pdf.starts_with(b"%PDF-"));
    let reparsed =
        crate::agent::tools::pdf_text::render(&pdf, "attached.pdf", None, 10_000).unwrap();
    assert!(reparsed.contains("2 pages"), "{reparsed}");
    assert!(
        !reparsed.contains("Real text"),
        "page 1 must not travel: {reparsed}"
    );
}

#[tokio::test]
async fn native_pdf_block_stays_closed_without_a_known_capable_model_and_transport() {
    let root = super::OwnedTestDirectory::new("pdf-native-closed");
    std::fs::write(root.path().join("scan.pdf"), scanned_pdf(1)).unwrap();
    // Unknown gateway model: images only, never a document.
    let unknown = native_tool(root.path(), "openai", "owned-unknown-gateway")
        .call(args(None))
        .await
        .unwrap();
    assert!(
        envelope(&unknown)
            .1
            .iter()
            .all(|part| part["type"] == "image"),
        "{unknown}"
    );
    // A model declared text-only gets neither.
    let text_only = native_tool(root.path(), "openai", "gpt-3.5-turbo")
        .call(args(None))
        .await
        .unwrap();
    assert!(serde_json::from_str::<serde_json::Value>(&text_only).is_err());
    // A catalogued vision model on a transport without a document channel.
    let chat = tool(root.path(), true).call(args(None)).await.unwrap();
    assert!(envelope(&chat).1.iter().all(|part| part["type"] == "image"));
}

#[tokio::test]
async fn pdf_block_reaches_each_provider_wire_format() {
    let root = super::OwnedTestDirectory::new("pdf-native-wire");
    std::fs::write(root.path().join("scan.pdf"), scanned_pdf(1)).unwrap();
    let output = native_tool(root.path(), "anthropic", "claude-fable-5-1")
        .call(args(None))
        .await
        .unwrap();
    let content = rig::message::ToolResultContent::from_tool_output(output);
    assert!(
        content
            .iter()
            .any(|part| matches!(part, rig::message::ToolResultContent::Document(_)))
    );
    let message = rig::message::Message::User {
        content: rig::OneOrMany::one(rig::message::UserContent::ToolResult(
            rig::message::ToolResult {
                id: "toolu_1".into(),
                call_id: Some("call_1".into()),
                content,
            },
        )),
    };
    let responses =
        Vec::<rig::providers::openai::responses_api::InputItem>::try_from(message.clone())
            .map(|items| serde_json::to_string(&items).unwrap());
    let gemini =
        rig::providers::gemini::completion::gemini_api_types::Content::try_from(message.clone())
            .map(|content| serde_json::to_value(&content).unwrap());
    let anthropic = rig::providers::anthropic::completion::Message::try_from(message).unwrap();
    let wire = serde_json::to_value(&anthropic).unwrap();
    let blocks = wire["content"][0]["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["type"], "document");
    assert_eq!(blocks[1]["source"]["type"], "base64");
    assert_eq!(blocks[1]["source"]["media_type"], "application/pdf");
    let responses = responses.unwrap();
    assert!(responses.contains("\"type\":\"input_file\""), "{responses}");
    assert!(
        responses.contains("data:application/pdf;base64,JVBERi"),
        "{responses}"
    );
    let gemini = gemini.unwrap().to_string();
    assert!(
        gemini.contains("\"mimeType\":\"application/pdf\""),
        "{gemini}"
    );
}

#[tokio::test]
async fn pdf_block_in_history_costs_pages_not_base64_text() {
    let root = super::OwnedTestDirectory::new("pdf-native-cost");
    std::fs::write(root.path().join("mixed.pdf"), mixed_pdf()).unwrap();
    let output = native_tool(root.path(), "anthropic", "claude-fable-5-1")
        .call(ReadArgs {
            path: "mixed.pdf".into(),
            ..ReadArgs::default()
        })
        .await
        .unwrap();
    let message = rig::message::Message::User {
        content: rig::OneOrMany::one(rig::message::UserContent::ToolResult(
            rig::message::ToolResult {
                id: "t".into(),
                call_id: None,
                content: rig::message::ToolResultContent::from_tool_output(output),
            },
        )),
    };
    let cost = crate::extras::subagents::image_cost::message_image_cost(&message);
    assert_eq!(cost.tokens, 2 * 3_000, "two attached pages");
    assert!(cost.encoded_chars > 0);
}
