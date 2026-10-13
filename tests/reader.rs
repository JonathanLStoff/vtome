//! Media opened from a reader is the media the file holds.

#![cfg(feature = "demux")]

use std::io::Cursor;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/bars_h264.mp4");

#[test]
fn a_reader_opens_what_the_file_does() {
    let from_file = vtome::open_media(FIXTURE).expect("open the fixture");
    let bytes = std::fs::read(FIXTURE).expect("read the fixture");
    let from_reader = vtome::open_media_reader(Box::new(Cursor::new(bytes)), "bars_h264.mp4").expect("open from a reader");

    let (file, reader) = (from_file.info(), from_reader.info());
    assert_eq!(reader.container, file.container);
    assert_eq!(reader.duration, file.duration);
    assert_eq!(reader.tracks.len(), file.tracks.len());
}

#[test]
fn bytes_that_are_no_container_are_refused() {
    let result = vtome::open_media_reader(Box::new(Cursor::new(vec![0_u8; 64])), "nothing");
    assert!(result.is_err());
}
