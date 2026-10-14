//! 验证 SSE 分段、编码、结束标记与资源限制

use serde_json::json;

use crate::sse::Decoder;

#[test]
fn decodes_every_possible_byte_split_including_utf8_and_crlf() {
    let input =
        "\u{feff}: keepalive\r\ndata: {\"text\":\"你好\"}\r\n\r\ndata: {\"finish\":true}\n\n"
            .as_bytes();
    for split in 0..=input.len() {
        let mut decoder = Decoder::default();
        let mut events = decoder.push(&input[..split]).unwrap();
        events.extend(decoder.push(&input[split..]).unwrap());
        decoder.finish().unwrap();
        assert_eq!(events, vec![json!({"text":"你好"}), json!({"finish":true})]);
    }
}

#[test]
fn supports_multiline_data_and_cr_only_lines() {
    let mut decoder = Decoder::default();
    assert_eq!(
        decoder
            .push(b"event: ignored\rdata: {\rdata: \"a\": 1}\r\r")
            .unwrap(),
        vec![json!({"a":1})]
    );
    decoder.finish().unwrap();
}

#[test]
fn rejects_truncated_invalid_and_oversized_events() {
    let mut decoder = Decoder::default();
    decoder.push(b"data: {}").unwrap();
    assert!(decoder.finish().is_err());
    assert!(Decoder::default().push(b"data: nope\n\n").is_err());
    assert!(
        Decoder::default()
            .push(&vec![b'a'; 512 * 1024 + 1])
            .is_err()
    );
}

#[test]
fn does_not_accept_data_after_done() {
    let mut decoder = Decoder::default();
    assert!(decoder.push(b"data: [DONE]\n\n").unwrap().is_empty());
    assert!(decoder.push(b"data: {}\n\n").is_err());
}
