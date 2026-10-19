use std::io::{Read, Write};
use std::sync::Arc;

use crate::keys::{BlobHasher, Keys};

pub const FRAME: usize = 1 << 20;
const LEGACY: u8 = 1;
const FRAMED: u8 = 2;
const MAX_SEALED_FRAME: usize = FRAME + (64 << 10);

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("{0}")]
    Broken(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn frame_aad(sid: &str, blob: &str, index: u64, last: bool) -> Vec<u8> {
    let tail = if last { "last" } else { "more" };
    format!("mush-sync blob {sid} {blob} frame {index} {tail}").into_bytes()
}

pub struct Sealer<R> {
    keys: Arc<Keys>,
    sid: String,
    blob: String,
    reader: R,
    len: u64,
    read: u64,
    index: u64,
    started: bool,
    done: bool,
    hasher: BlobHasher,
}

impl<R: Read> Sealer<R> {
    pub fn new(keys: Arc<Keys>, sid: &str, blob: &str, reader: R, len: u64) -> Self {
        let hasher = keys.blob_hasher(len);
        Self {
            keys,
            sid: sid.to_owned(),
            blob: blob.to_owned(),
            reader,
            len,
            read: 0,
            index: 0,
            started: false,
            done: false,
            hasher,
        }
    }

    fn frame(&mut self) -> std::io::Result<Vec<u8>> {
        let want = usize::try_from((self.len - self.read).min(FRAME as u64)).unwrap_or(FRAME);
        let mut plain = vec![0u8; want];
        self.reader.read_exact(&mut plain)?;
        self.read += want as u64;
        self.hasher.update(&plain);
        let last = self.read == self.len;
        if last && self.hasher.finish().as_deref() != Some(self.blob.as_str()) {
            return Err(std::io::Error::other(
                "the file changed while it was uploaded, retrying next pass",
            ));
        }
        let sealed = self
            .keys
            .seal(&frame_aad(&self.sid, &self.blob, self.index, last), &plain)?;
        let mut out = Vec::with_capacity(sealed.len() + 6);
        if !self.started {
            out.push(FRAMED);
            self.started = true;
        }
        let size = u32::try_from(sealed.len()).map_err(std::io::Error::other)?;
        out.extend_from_slice(&size.to_le_bytes());
        out.push(u8::from(last));
        out.extend_from_slice(&sealed);
        self.index += 1;
        self.done = last;
        Ok(out)
    }
}

impl<R: Read> Iterator for Sealer<R> {
    type Item = std::io::Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let frame = self.frame();
        if frame.is_err() {
            self.done = true;
        }
        Some(frame)
    }
}

pub fn seal_all(keys: &Arc<Keys>, sid: &str, blob: &str, bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len() / 2 + 64);
    for frame in Sealer::new(Arc::clone(keys), sid, blob, bytes, bytes.len() as u64) {
        out.extend_from_slice(&frame?);
    }
    Ok(out)
}

enum Mode {
    Start,
    Legacy,
    Framed,
    Done,
}

pub struct Unsealer<'k> {
    keys: &'k Keys,
    sid: &'k str,
    blob: &'k str,
    pending: Vec<u8>,
    index: u64,
    mode: Mode,
    hasher: BlobHasher,
    written: u64,
    len: u64,
}

impl<'k> Unsealer<'k> {
    #[must_use]
    pub fn new(keys: &'k Keys, sid: &'k str, blob: &'k str, len: u64) -> Self {
        Self {
            keys,
            sid,
            blob,
            pending: Vec::new(),
            index: 0,
            mode: Mode::Start,
            hasher: keys.blob_hasher(len),
            written: 0,
            len,
        }
    }

    fn emit(&mut self, plain: &[u8], sink: &mut dyn Write) -> Result<(), BlobError> {
        self.written += plain.len() as u64;
        if self.written > self.len {
            return Err(BlobError::Broken(format!(
                "blob {} is longer than its manifest says",
                self.blob
            )));
        }
        self.hasher.update(plain);
        sink.write_all(plain)?;
        Ok(())
    }

    pub fn feed(&mut self, bytes: &[u8], sink: &mut dyn Write) -> Result<(), BlobError> {
        self.pending.extend_from_slice(bytes);
        loop {
            match self.mode {
                Mode::Start => match self.pending.first() {
                    None => return Ok(()),
                    Some(&LEGACY) => self.mode = Mode::Legacy,
                    Some(&FRAMED) => {
                        self.pending.remove(0);
                        self.mode = Mode::Framed;
                    }
                    Some(_) => return Err(self.broken("has an unknown format")),
                },
                Mode::Legacy => {
                    if self.pending.len() as u64 > self.len.saturating_mul(2) + (1 << 20) {
                        return Err(self.broken("is far larger than its manifest says"));
                    }
                    return Ok(());
                }
                Mode::Framed => {
                    let Some(header) = self.pending.get(..5) else {
                        return Ok(());
                    };
                    let size = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
                    let size = usize::try_from(size).unwrap_or(usize::MAX);
                    let last = match header[4] {
                        0 => false,
                        1 => true,
                        _ => return Err(self.broken("has a damaged frame")),
                    };
                    if size > MAX_SEALED_FRAME {
                        return Err(self.broken("has an oversized frame"));
                    }
                    if self.pending.len() < 5 + size {
                        return Ok(());
                    }
                    let plain = self
                        .keys
                        .open(
                            &frame_aad(self.sid, self.blob, self.index, last),
                            &self.pending[5..5 + size],
                        )
                        .map_err(|error| self.broken(&error.to_string()))?;
                    self.pending.drain(..5 + size);
                    self.emit(&plain, sink)?;
                    self.index += 1;
                    if last {
                        self.mode = Mode::Done;
                    }
                }
                Mode::Done => {
                    if self.pending.is_empty() {
                        return Ok(());
                    }
                    return Err(self.broken("has bytes after its last frame"));
                }
            }
        }
    }

    pub fn finish(mut self, sink: &mut dyn Write) -> Result<(), BlobError> {
        match self.mode {
            Mode::Legacy => {
                let sealed = std::mem::take(&mut self.pending);
                let plain = self
                    .keys
                    .open(&crate::protocol::blob_aad(self.sid, self.blob), &sealed)
                    .map_err(|error| self.broken(&error.to_string()))?;
                self.emit(&plain, sink)?;
            }
            Mode::Done => {}
            Mode::Start | Mode::Framed => return Err(self.broken("was cut short")),
        }
        if self.hasher.finish().as_deref() != Some(self.blob) {
            return Err(self.broken("does not match its name"));
        }
        Ok(())
    }

    fn broken(&self, why: &str) -> BlobError {
        BlobError::Broken(format!("blob {} {why}", self.blob))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_all(
        keys: &Keys,
        sid: &str,
        blob: &str,
        len: u64,
        sealed: &[u8],
    ) -> Result<Vec<u8>, BlobError> {
        let mut plain = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
        let mut unsealer = Unsealer::new(keys, sid, blob, len);
        unsealer.feed(sealed, &mut plain)?;
        unsealer.finish(&mut plain)?;
        Ok(plain)
    }

    fn keys() -> Arc<Keys> {
        Arc::new(Keys::from_master(&[5; 32]))
    }

    #[test]
    fn a_framed_blob_round_trips_in_pieces_of_any_size() {
        let keys = keys();
        let bytes: Vec<u8> = (0..(FRAME * 2 + 12_345)).map(|i| (i % 251) as u8).collect();
        let blob = keys.blob_id(&bytes);
        let sealed = seal_all(&keys, "s", &blob, &bytes).unwrap();
        for piece in [1, 7, 4096, FRAME + 3] {
            let mut out = Vec::new();
            let mut unsealer = Unsealer::new(&keys, "s", &blob, bytes.len() as u64);
            for part in sealed.chunks(piece) {
                unsealer.feed(part, &mut out).unwrap();
            }
            unsealer.finish(&mut out).unwrap();
            assert_eq!(out, bytes);
        }
    }

    #[test]
    fn an_empty_file_is_one_last_frame() {
        let keys = keys();
        let blob = keys.blob_id(b"");
        let sealed = seal_all(&keys, "s", &blob, b"").unwrap();
        assert!(open_all(&keys, "s", &blob, 0, &sealed).unwrap().is_empty());
    }

    #[test]
    fn truncated_reordered_or_misnamed_frames_are_refused() {
        let keys = keys();
        let bytes = vec![9u8; FRAME + 10];
        let blob = keys.blob_id(&bytes);
        let sealed = seal_all(&keys, "s", &blob, &bytes).unwrap();
        let first_frame = 1 + 5 + u32::from_le_bytes(sealed[1..5].try_into().unwrap()) as usize;
        assert!(
            open_all(
                &keys,
                "s",
                &blob,
                bytes.len() as u64,
                &sealed[..first_frame]
            )
            .is_err()
        );
        let mut flipped = sealed.clone();
        flipped[5] = 1;
        assert!(open_all(&keys, "s", &blob, bytes.len() as u64, &flipped).is_err());
        assert!(open_all(&keys, "other", &blob, bytes.len() as u64, &sealed).is_err());
        let mut longer = sealed.clone();
        longer.push(0);
        assert!(open_all(&keys, "s", &blob, bytes.len() as u64, &longer).is_err());
        assert!(open_all(&keys, "s", &blob, bytes.len() as u64 + 1, &sealed).is_err());
    }

    #[test]
    fn a_file_that_changes_under_the_upload_never_finishes_it() {
        let keys = keys();
        let blob = keys.blob_id(b"what the manifest promised");
        let frames: Vec<_> = Sealer::new(
            Arc::clone(&keys),
            "s",
            &blob,
            &b"something else entirely, same len"[..26],
            26,
        )
        .collect();
        assert!(frames.last().unwrap().is_err());
    }

    #[test]
    fn whole_blobs_from_before_framing_still_open() {
        let keys = keys();
        let bytes = b"sealed the old way".to_vec();
        let blob = keys.blob_id(&bytes);
        let sealed = keys
            .seal(&crate::protocol::blob_aad("s", &blob), &bytes)
            .unwrap();
        assert_eq!(
            open_all(&keys, "s", &blob, bytes.len() as u64, &sealed).unwrap(),
            bytes
        );
    }
}
