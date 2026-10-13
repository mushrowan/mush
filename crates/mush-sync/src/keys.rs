use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

const SEALED_VERSION: u8 = 1;
const NONCE_LEN: usize = 24;
const MAX_OPENED: u64 = 1 << 30;

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("could not read the sync key at {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the sync key at {0} is not 32 bytes of base64")]
    Malformed(PathBuf),
}

#[derive(Debug, thiserror::Error)]
#[error("sealed data did not open, the key differs or the data was tampered with")]
pub struct OpenError;

pub struct Keys {
    cipher: XChaCha20Poly1305,
    ids: [u8; 32],
    content: [u8; 32],
    token: String,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keys(..)")
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

impl Keys {
    #[must_use]
    pub fn from_master(master: &[u8; 32]) -> Self {
        let derive = |context: &str| blake3::derive_key(context, master);
        Self {
            cipher: XChaCha20Poly1305::new(&Key::from(derive("mush-sync 2026-10 encryption"))),
            ids: derive("mush-sync 2026-10 session ids"),
            content: derive("mush-sync 2026-10 content"),
            token: URL_SAFE_NO_PAD.encode(derive("mush-sync 2026-10 server auth")),
        }
    }

    pub fn generate_master() -> std::io::Result<String> {
        let mut master = [0u8; 32];
        getrandom::fill(&mut master).map_err(std::io::Error::other)?;
        Ok(STANDARD.encode(master))
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let bytes = STANDARD
            .decode(text)
            .or_else(|_| URL_SAFE_NO_PAD.decode(text))
            .ok()?;
        let master: [u8; 32] = bytes.try_into().ok()?;
        Some(Self::from_master(&master))
    }

    pub fn load(path: &Path) -> Result<Self, KeyError> {
        let text = std::fs::read_to_string(path).map_err(|source| KeyError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text).ok_or_else(|| KeyError::Malformed(path.to_owned()))
    }

    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    #[must_use]
    pub fn token_hash(&self) -> String {
        token_hash(&self.token)
    }

    #[must_use]
    pub fn sid(&self, session: &str) -> String {
        hex(&blake3::keyed_hash(&self.ids, session.as_bytes()).as_bytes()[..16])
    }

    fn keyed(&self, parts: &[&[u8]]) -> blake3::Hash {
        let mut hasher = blake3::Hasher::new_keyed(&self.content);
        for part in parts {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
        hasher.finalize()
    }

    #[must_use]
    pub fn blob_id(&self, bytes: &[u8]) -> String {
        hex(&self.keyed(&[b"blob", bytes]).as_bytes()[..16])
    }

    #[must_use]
    pub fn fork_id(&self, session: &str, machine: &str, digest: &str) -> String {
        hex(&self
            .keyed(&[
                b"fork",
                session.as_bytes(),
                machine.as_bytes(),
                digest.as_bytes(),
            ])
            .as_bytes()[..8])
    }

    #[must_use]
    pub fn chain_start(&self, session: &str, generation: u64) -> String {
        self.keyed(&[b"chain", session.as_bytes(), &generation.to_le_bytes()])
            .to_hex()
            .to_string()
    }

    #[must_use]
    pub fn chain_next(&self, chain: &str, bytes: &[u8]) -> String {
        self.keyed(&[b"link", chain.as_bytes(), bytes])
            .to_hex()
            .to_string()
    }

    pub fn seal(&self, aad: &[u8], plaintext: &[u8]) -> std::io::Result<Vec<u8>> {
        use std::io::Write;
        let mut packed =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        packed.write_all(plaintext)?;
        let packed = packed.finish()?;
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(std::io::Error::other)?;
        let sealed = self
            .cipher
            .encrypt(&XNonce::from(nonce), Payload { msg: &packed, aad })
            .map_err(|_| std::io::Error::other("encryption failed"))?;
        let mut out = Vec::with_capacity(1 + NONCE_LEN + sealed.len());
        out.push(SEALED_VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    pub fn open(&self, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, OpenError> {
        let [SEALED_VERSION, rest @ ..] = sealed else {
            return Err(OpenError);
        };
        let (nonce, body) = rest.split_at_checked(NONCE_LEN).ok_or(OpenError)?;
        let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| OpenError)?;
        let packed = self
            .cipher
            .decrypt(&XNonce::from(nonce), Payload { msg: body, aad })
            .map_err(|_| OpenError)?;
        let mut plaintext = Vec::new();
        flate2::read::DeflateDecoder::new(packed.as_slice())
            .take(MAX_OPENED)
            .read_to_end(&mut plaintext)
            .map_err(|_| OpenError)?;
        Ok(plaintext)
    }
}

#[must_use]
pub fn token_hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Keys {
        Keys::from_master(&[7; 32])
    }

    #[test]
    fn sealed_data_opens_only_with_the_same_key_and_context() {
        let keys = keys();
        let sealed = keys.seal(b"chunk a 0", b"secret history line").unwrap();
        assert!(
            !sealed
                .windows(b"secret".len())
                .any(|window| window == b"secret")
        );
        assert_eq!(
            keys.open(b"chunk a 0", &sealed).unwrap(),
            b"secret history line"
        );
        assert!(keys.open(b"chunk a 1", &sealed).is_err());
        assert!(
            Keys::from_master(&[8; 32])
                .open(b"chunk a 0", &sealed)
                .is_err()
        );
        let mut flipped = sealed.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert!(keys.open(b"chunk a 0", &flipped).is_err());
    }

    #[test]
    fn derived_names_hide_the_session_and_depend_on_the_key() {
        let keys = keys();
        let sid = keys.sid("abc123");
        assert_eq!(sid.len(), 32);
        assert!(!sid.contains("abc123"));
        assert_eq!(sid, keys.sid("abc123"));
        assert_ne!(sid, Keys::from_master(&[8; 32]).sid("abc123"));
        assert_ne!(keys.blob_id(b"one"), keys.blob_id(b"two"));
        assert_eq!(keys.fork_id("abc", "dot", "x").len(), 16);
    }

    #[test]
    fn the_chain_commits_to_content_and_order() {
        let keys = keys();
        let start = keys.chain_start("s", 0);
        let ab = keys.chain_next(&keys.chain_next(&start, b"a"), b"b");
        let ba = keys.chain_next(&keys.chain_next(&start, b"b"), b"a");
        assert_ne!(ab, ba);
        assert_ne!(start, keys.chain_start("s", 1));
    }

    #[test]
    fn a_master_key_round_trips_through_its_text_form() {
        let text = Keys::generate_master().unwrap();
        let first = Keys::parse(&format!("{text}\n")).unwrap();
        let second = Keys::parse(&text).unwrap();
        assert_eq!(first.token(), second.token());
        assert_eq!(first.token_hash(), token_hash(second.token()));
        assert!(Keys::parse("not a key").is_none());
        assert_eq!(format!("{first:?}"), "Keys(..)");
    }
}
