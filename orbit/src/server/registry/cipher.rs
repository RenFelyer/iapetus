//! Шифрование секретных значений в базе: XChaCha20-Poly1305 с ключом из файла `0600`.
//!
//! Запись — случайный nonce (24 байта) и шифротекст с тегом. AAD привязывает шифротекст к
//! его записи: значение, перенесённое в базе в чужую запись, не расшифруется.

use std::fmt;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{Key, KeyInit, XChaCha20Poly1305, XNonce};

use super::Error;
use super::secret::{Token, random};

const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;

pub(crate) struct TokenCipher(XChaCha20Poly1305);

impl fmt::Debug for TokenCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenCipher(***)")
    }
}

impl TokenCipher {
    pub(crate) fn from_key(key: [u8; KEY_LEN]) -> Self {
        Self(XChaCha20Poly1305::new(&Key::from(key)))
    }

    /// Читает ключ из файла; если файла нет, создаёт его со случайным ключом и правами `0600`.
    /// Файл с правами шире `0600` или не из 32 байт — ошибка: молча чинить ключ нельзя.
    pub(crate) fn load_or_create(path: &Path) -> Result<Self, Error> {
        match OpenOptions::new().read(true).open(path) {
            Ok(mut file) => {
                if file.metadata()?.permissions().mode() & 0o077 != 0 {
                    return Err(Error::KeyFile("права файла ключа шире 0600"));
                }
                let mut bytes = Vec::new();
                Read::by_ref(&mut file).take(KEY_LEN as u64 + 1).read_to_end(&mut bytes)?;
                let key = bytes.try_into().map_err(|_| Error::KeyFile("ключ должен быть ровно 32 байта"))?;
                Ok(Self::from_key(key))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = random::<KEY_LEN>()?;
                let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
                file.write_all(&key)?;
                file.sync_all()?;
                Ok(Self::from_key(key))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub(super) fn seal(&self, token: &Token, aad: &[u8]) -> Result<Vec<u8>, Error> {
        let nonce = random::<NONCE_LEN>()?;
        let payload = Payload {
            msg: token.expose().as_bytes(),
            aad,
        };
        let sealed = self.0.encrypt(&XNonce::from(nonce), payload).map_err(|_| Error::Crypto)?;
        Ok([nonce.as_slice(), &sealed].concat())
    }

    pub(super) fn open(&self, blob: &[u8], aad: &[u8]) -> Result<Token, Error> {
        let (nonce, sealed) = blob.split_at_checked(NONCE_LEN).ok_or(Error::Crypto)?;
        let nonce = XNonce::try_from(nonce).map_err(|_| Error::Crypto)?;
        let plain = self.0.decrypt(&nonce, Payload { msg: sealed, aad }).map_err(|_| Error::Crypto)?;
        String::from_utf8(plain).map(Token::new).map_err(|_| Error::Crypto)
    }
}
