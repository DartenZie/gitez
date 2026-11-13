//! Images sent with a prompt, through the supervisor's pipeline with the
//! fake `claude`: how the agent is given them, what the transcript keeps,
//! and that a retry sends them again.
#![cfg(unix)]

mod common;

use base64::Engine as _;
use common::Fixture;
use gitez_agent_thread::{Ending, PreparedImage, RetryPlan, Timestamp, TurnKind, prepare_image};
use gitez_supervisor::Command;
use serde_json::Value;

fn image(width: u32, height: u32, rgb: [u8; 3]) -> PreparedImage {
    let pixels = image::RgbImage::from_pixel(width, height, image::Rgb(rgb));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(pixels)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("encode");
    prepare_image(&bytes.into_inner()).expect("prepared")
}

/// The image blocks' media types and decoded bytes, and the text, of the
/// stream-JSON message `stdin` holds.
fn message(stdin: &str) -> (Vec<(String, Vec<u8>)>, Option<String>) {
    assert_eq!(stdin.lines().count(), 1, "one message");
    let message: Value = serde_json::from_str(stdin).expect("a stream-JSON message");
    assert_eq!(message["type"], "user");
    let mut images = Vec::new();
    let mut text = None;
    for block in message["message"]["content"].as_array().expect("content") {
        match block["type"].as_str() {
            Some("image") => images.push((
                block["source"]["media_type"]
                    .as_str()
                    .expect("type")
                    .to_owned(),
                base64::engine::general_purpose::STANDARD
                    .decode(block["source"]["data"].as_str().expect("data"))
                    .expect("base64"),
            )),
            Some("text") => text = Some(block["text"].as_str().expect("text").to_owned()),
            other => panic!("unexpected block {other:?}"),
        }
    }
    (images, text)
}

#[test]
fn a_prompt_with_images_is_one_stream_json_message_and_the_transcript_keeps_no_data() {
    let fixture = Fixture::new();
    let images = [image(30, 20, [200, 0, 0]), image(12, 12, [0, 0, 200])];
    let created = fixture.create_with_images("SEE-IMAGE what is in these?", &images);
    let id = created.id.clone();
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::Answer), "{thread:#?}");
    assert_eq!(thread.final_message(), Some("I see the image."));

    let calls = fixture.calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].contains("--input-format stream-json"),
        "{}",
        calls[0]
    );
    let (sent, text) = message(&fixture.stdin(1));
    assert_eq!(text.as_deref(), Some("SEE-IMAGE what is in these?"));
    let expected: Vec<(String, Vec<u8>)> = images
        .iter()
        .map(|image| ("image/png".to_owned(), image.bytes.clone()))
        .collect();
    assert_eq!(sent, expected);

    // The header names the images; no line holds their data, though the
    // fake echoed the message and a tool's image.
    let transcripts = fixture.transcripts(&id);
    let (header, lines) = &transcripts[0];
    let names: Vec<&str> = thread.turns[0]
        .images
        .iter()
        .map(|image| image.name.as_str())
        .collect();
    assert_eq!(header.images, names);
    assert_eq!(header.prompt, "SEE-IMAGE what is in these?");
    let path = &fixture.store().runs(&id).expect("runs")[0];
    let raw = std::fs::read_to_string(path).expect("read transcript");
    for image in &images {
        let data = base64::engine::general_purpose::STANDARD.encode(&image.bytes);
        assert!(!raw.contains(&data), "the transcript holds an image");
    }
    assert!(!raw.contains(&"Q".repeat(300)));
    assert!(
        lines
            .iter()
            .any(|line| line.contains("[image omitted by GitEZ]"))
    );
}

#[test]
fn a_prompt_without_images_is_sent_as_before() {
    let fixture = Fixture::new();
    let id = fixture.create("What is the answer?").id;
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::Answer));
    assert!(!fixture.calls()[0].contains("--input-format"));
    assert_eq!(fixture.stdin(1), "What is the answer?");
    assert!(fixture.transcripts(&id)[0].0.images.is_empty());
}

#[test]
fn a_reply_with_an_image_continues_the_conversation() {
    let fixture = Fixture::new();
    let id = fixture.create("What is the answer?").id;
    let first = fixture.run(&id, Command::Start);
    let session = first.session_id.clone().expect("session");
    let store = fixture.store();
    let reply = image(9, 9, [0, 180, 0]);
    let listed = store
        .add_images(&id, std::slice::from_ref(&reply), Timestamp::now())
        .expect("add images");
    store
        .update(&id, |thread| {
            thread.request_turn_with_images(
                TurnKind::Reply,
                String::new(),
                listed.clone(),
                Timestamp::now(),
            )
        })
        .expect("reply");
    let thread = fixture.run(&id, Command::Reply);
    assert_eq!(thread.ending, Some(Ending::Answer), "{thread:#?}");
    let calls = fixture.calls();
    assert_eq!(calls.len(), 2);
    assert!(
        calls[1].contains(&format!("--resume {session}")),
        "{}",
        calls[1]
    );
    assert!(calls[1].contains("--input-format stream-json"));
    // No words: only the image.
    let (sent, text) = message(&fixture.stdin(2));
    assert_eq!(sent, [("image/png".to_owned(), reply.bytes)]);
    assert_eq!(text, None);
    assert_eq!(thread.turns[1].images, listed);
}

#[test]
fn a_retry_sends_the_images_again() {
    let fixture = Fixture::new();
    let images = [image(16, 8, [10, 20, 30])];
    let id = fixture
        .create_with_images("FAIL-NO-INIT with a picture", &images)
        .id;
    let thread = fixture.run(&id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::Failed));
    let plan = fixture
        .store()
        .update(&id, |thread| {
            thread.request_retry(thread.network, Timestamp::now())
        })
        .expect("retry");
    assert_eq!(plan, RetryPlan::RunAgain);
    let thread = fixture.run(&id, Command::Retry);
    assert_eq!(thread.ending, Some(Ending::Answer), "{thread:#?}");
    assert_eq!(thread.turns[1].images, thread.turns[0].images);
    for run in [1, 2] {
        let (sent, text) = message(&fixture.stdin(run));
        assert_eq!(sent, [("image/png".to_owned(), images[0].bytes.clone())]);
        assert_eq!(text.as_deref(), Some("FAIL-NO-INIT with a picture"));
    }
}

#[test]
fn a_missing_image_fails_the_turn_before_the_agent_runs() {
    let fixture = Fixture::new();
    let created = fixture.create_with_images("SEE-IMAGE", &[image(4, 4, [1, 2, 3])]);
    let store = fixture.store();
    let path = store
        .image_path(&created.id, &created.turns[0].images[0])
        .expect("path");
    std::fs::remove_file(path).expect("remove");
    let thread = fixture.run(&created.id, Command::Start);
    assert_eq!(thread.ending, Some(Ending::Failed));
    let error = thread.error.expect("error");
    assert!(
        error.message.contains("could not read the image"),
        "{error:?}"
    );
    assert!(fixture.calls().is_empty());
}
