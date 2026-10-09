use loom_eval_b9::{STREAM_HEAD_BYTES, STREAM_TAIL_BYTES, read_capped};
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn small_stream_passes_through_whole() {
    let (mut writer, reader) = tokio::io::duplex(64);
    writer.write_all(b"hello").await.unwrap();
    drop(writer);
    assert_eq!(read_capped(reader).await.unwrap(), b"hello");
}

#[tokio::test]
async fn oversized_stream_keeps_head_tail_and_marker() {
    let (mut writer, reader) = tokio::io::duplex(64);
    let payload: Vec<u8> = (0..100_000).map(|i| (i % 251) as u8).collect();
    // Produce concurrently: the duplex buffer is tiny, so a sequential
    // write-then-read would deadlock on a full pipe.
    let producer = tokio::spawn(async move {
        writer.write_all(&payload).await.unwrap();
    });
    let kept = read_capped(reader).await.unwrap();
    producer.await.unwrap();
    let payload: Vec<u8> = (0..100_000).map(|i| (i % 251) as u8).collect();
    let marker = b"... [";
    let cut = kept
        .windows(marker.len())
        .position(|w| w == marker)
        .expect("omission marker present");
    let (head_part, rest) = kept.split_at(cut);
    assert_eq!(head_part, &payload[..head_part.len()]);
    assert!(head_part.len() <= STREAM_HEAD_BYTES);
    // The marker names exactly the dropped middle…
    let marker_end = rest
        .iter()
        .position(|&b| b == b'\n')
        .expect("marker ends with newline");
    let marker_text = String::from_utf8_lossy(&rest[..marker_end]).into_owned();
    let omitted = 100_000 - head_part.len() - STREAM_TAIL_BYTES;
    assert_eq!(marker_text, format!("... [{omitted} bytes omitted] ..."));
    // …and the tail matches the end of the payload.
    let tail_part = &rest[marker_end + 1..];
    assert_eq!(tail_part, &payload[100_000 - STREAM_TAIL_BYTES..]);
}

#[tokio::test]
async fn the_tail_line_of_a_long_listing_survives() {
    // A failing command's diagnosis lands at the tail; it must survive
    // the cap even when the head fills first.
    let (mut writer, reader) = tokio::io::duplex(64);
    let listing: String = (1..=20_000).map(|i| format!("line {i:06}\n")).collect();
    let bytes = listing.into_bytes();
    let producer = tokio::spawn(async move {
        writer.write_all(&bytes).await.unwrap();
    });
    let kept = read_capped(reader).await.unwrap();
    producer.await.unwrap();
    let text = String::from_utf8_lossy(&kept).into_owned();
    assert!(
        text.contains("line 020000\n"),
        "the final line must survive: {text}"
    );
}
