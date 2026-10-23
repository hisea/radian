//! Image inputs through the `read` tool (ADR 0018, backlog `images.md` Q3/Q4/Q6): the magic
//! number decides the kind, the two limits are enforced, the text says what the model sees,
//! and the parts map to an Anthropic `image` block.

mod common;

use std::sync::Arc;

use agent_provider_anthropic::testing::{
    MockServer, Reply, block_stop, message_delta, message_start, message_stop, text_delta,
    text_start,
};
use agent_provider_anthropic::{Anthropic, AnthropicConfig, ApiKey};
use common::{TempDir, call, text};
use radian_coding::{CodingTools, MAX_IMAGE_BYTES, MAX_IMAGE_SIDE, ReadRoots};
use radian_core::{ContentPart, Model, ModelContext, ModelRequest, NoopSink};
use serde_json::{Value, json};

/// A minimal PNG header with these dimensions (24 bytes: signature, IHDR, width, height).
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes
}

fn tools(root: &std::path::Path) -> CodingTools {
    CodingTools::with_read_roots(root, ReadRoots::none()).unwrap()
}

fn blind(root: &std::path::Path) -> CodingTools {
    tools(root).with_image_support(Arc::new(|| false))
}

fn workspace(label: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new(label);
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    (dir, root)
}

#[tokio::test]
async fn read_of_a_png_returns_the_text_line_and_the_image_part() {
    let (_dir, root) = workspace("read-png");
    let bytes = png(1200, 800);
    std::fs::write(root.join("shot.png"), &bytes).unwrap();
    let output = call(&tools(&root), "read", json!({"path": "shot.png"}))
        .await
        .unwrap();
    assert_eq!(output.content.len(), 2, "{:?}", output.content);
    assert_eq!(text(&output), "Read image file [image/png] 1200×800");
    match &output.content[1] {
        ContentPart::Image {
            mime_type, data, ..
        } => {
            assert_eq!(mime_type, "image/png");
            assert_eq!(data, &radian_coding::image::base64_encode(&bytes));
        }
        other => panic!("expected the image part, got {other:?}"),
    }
    assert_eq!(output.structured.as_ref().unwrap()["image"]["width"], 1200);
}

#[tokio::test]
async fn a_model_known_blind_gets_only_the_text_and_the_omission_line() {
    let (_dir, root) = workspace("read-blind");
    std::fs::write(root.join("shot.png"), png(1200, 800)).unwrap();
    let output = call(&blind(&root), "read", json!({"path": "shot.png"}))
        .await
        .unwrap();
    assert_eq!(output.content.len(), 1, "{:?}", output.content);
    let body = text(&output);
    assert!(
        body.contains("Read image file [image/png] 1200×800"),
        "{body}"
    );
    assert!(
        body.contains(
            "[Current model does not support images. The image will be omitted from this request.]"
        ),
        "{body}"
    );
}

#[tokio::test]
async fn a_long_side_adds_the_provider_scaling_note() {
    let (_dir, root) = workspace("read-long");
    std::fs::write(root.join("long.png"), png(2000, 100)).unwrap();
    let output = call(&tools(&root), "read", json!({"path": "long.png"}))
        .await
        .unwrap();
    assert!(
        text(&output).contains("scales it down"),
        "{}",
        text(&output)
    );
}

#[tokio::test]
async fn the_byte_and_pixel_limits_are_image_too_large() {
    let (_dir, root) = workspace("read-limits");
    let mut huge = png(10, 10);
    huge.resize(MAX_IMAGE_BYTES as usize + 1, 0);
    std::fs::write(root.join("huge.png"), &huge).unwrap();
    let failure = call(&tools(&root), "read", json!({"path": "huge.png"}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "image_too_large", "{failure:?}");
    assert!(failure.message.contains("5 MiB"), "{failure:?}");

    std::fs::write(root.join("wide.png"), png(MAX_IMAGE_SIDE + 1, 10)).unwrap();
    let failure = call(&tools(&root), "read", json!({"path": "wide.png"}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "image_too_large", "{failure:?}");
    assert!(failure.message.contains("8000"), "{failure:?}");
}

#[tokio::test]
async fn offset_or_limit_on_an_image_is_image_offset() {
    let (_dir, root) = workspace("read-offset");
    std::fs::write(root.join("shot.png"), png(10, 10)).unwrap();
    for arguments in [
        json!({"path": "shot.png", "offset": 2}),
        json!({"path": "shot.png", "limit": 2}),
    ] {
        let failure = call(&tools(&root), "read", arguments).await.unwrap_err();
        assert_eq!(failure.code, "image_offset", "{failure:?}");
    }
}

#[tokio::test]
async fn the_extension_never_decides_the_kind() {
    let (_dir, root) = workspace("read-kind");
    // A `.png` that is really text stays text.
    std::fs::write(root.join("notes.png"), "hello\n").unwrap();
    let output = call(&tools(&root), "read", json!({"path": "notes.png"}))
        .await
        .unwrap();
    assert_eq!(output.content.len(), 1);
    assert_eq!(text(&output), "1: hello\n");

    // A PNG named `.txt` is an image.
    std::fs::write(root.join("shot.txt"), png(10, 10)).unwrap();
    let output = call(&tools(&root), "read", json!({"path": "shot.txt"}))
        .await
        .unwrap();
    assert_eq!(output.content.len(), 2, "{:?}", output.content);
    assert!(matches!(output.content[1], ContentPart::Image { .. }));

    // BMP is deliberately not an image: the old binary refusal.
    let mut bmp = b"BM\xFF\xFE".to_vec();
    bmp.resize(64, 0);
    std::fs::write(root.join("old.bmp"), &bmp).unwrap();
    let failure = call(&tools(&root), "read", json!({"path": "old.bmp"}))
        .await
        .unwrap_err();
    assert_eq!(failure.code, "not_utf8", "{failure:?}");
}

/// Q3/Q6 acceptance: the read tool's parts become an Anthropic `image` block. The mock server
/// is the adapter's own testing helper; this is the provider's real request builder.
#[tokio::test]
async fn the_read_parts_become_an_anthropic_image_block() {
    let (_dir, root) = workspace("read-anthropic");
    let bytes = png(1200, 800);
    std::fs::write(root.join("shot.png"), &bytes).unwrap();
    let output = call(&tools(&root), "read", json!({"path": "shot.png"}))
        .await
        .unwrap();

    let server = MockServer::start(vec![Reply::sse(&[
        message_start(None),
        text_start(0),
        text_delta(0, "ok"),
        block_stop(0),
        message_delta("end_turn", None),
        message_stop(),
    ])])
    .await;
    let config = AnthropicConfig::new(server.base_url.clone(), "contract-model")
        .with_api_key(ApiKey::new("sk-ant-test"));
    let model = Anthropic::new(config).expect("a local mock config is valid");
    let request = ModelRequest {
        system_prompt: None,
        messages: vec![
            radian_core::Message::user("look at shot.png"),
            radian_core::Message::Assistant {
                content: Vec::new(),
                reasoning: Vec::new(),
                tool_calls: vec![radian_core::ToolCall {
                    id: "toolu_1".into(),
                    name: "read".into(),
                    arguments: json!({"path": "shot.png"}),
                }],
                provider_payload: None,
                interrupted: false,
                meta: Default::default(),
            },
            radian_core::Message::Tool {
                result: radian_core::ToolResult {
                    call_id: "toolu_1".into(),
                    content: output.content,
                    structured: None,
                    is_error: false,
                    error_code: None,
                },
                meta: Default::default(),
            },
        ],
        tools: Vec::new(),
        settings: Value::Null,
    };
    let cx = ModelContext {
        cancellation: radian_core::CancellationToken::new(),
        events: &NoopSink,
    };
    model.complete(request, cx).await.unwrap();

    let body = server.requests()[0].json();
    let result = &body["messages"][2]["content"][0];
    assert_eq!(result["type"], "tool_result", "{body}");
    let blocks = result["content"]
        .as_array()
        .expect("the content-block array");
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "Read image file [image/png] 1200×800");
    assert_eq!(blocks[1]["type"], "image", "{body}");
    assert_eq!(blocks[1]["source"]["type"], "base64");
    assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    assert_eq!(
        blocks[1]["source"]["data"],
        radian_coding::image::base64_encode(&bytes)
    );
}
