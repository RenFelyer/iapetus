//! Тесты реестра на настоящей SQLite во временном каталоге.

use nerve::fingerprint::Fingerprint;
use nerve::session_state::SessionState;
use russh::keys::ssh_key::{Algorithm, PrivateKey};
use tempfile::TempDir;

use super::sessions::new_fingerprint;
use super::*;

#[path = "registry/scenarios.rs"]
mod scenarios;

/// Ключ шифрования во всех тестах, где важно переоткрыть базу тем же ключом.
const TEST_KEY: [u8; 32] = [7; 32];

pub(super) struct TestNode {
    pub(super) fingerprint: Fingerprint,
    pub(super) key: PrivateKey,
}

pub(super) fn random_key() -> PrivateKey {
    PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap()
}

pub(super) struct Fixture {
    pub(super) dir: TempDir,
    pub(super) reg: Registry,
}

impl Fixture {
    pub(super) fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let reg = Registry::open(&dir.path().join("registry.db"), TokenCipher::from_key(TEST_KEY)).unwrap();
        Self { dir, reg }
    }

    fn db_path(&self) -> std::path::PathBuf {
        self.dir.path().join("registry.db")
    }

    pub(super) fn reopen(&mut self) {
        self.reg = Registry::open(&self.db_path(), TokenCipher::from_key(TEST_KEY)).unwrap();
    }

    /// Runner с новым fingerprint и ключом; закрытый ключ нужен тестам, которые подписывают за узел.
    pub(super) fn node(&mut self, name: &str) -> TestNode {
        let key = random_key();
        let node = NewNode {
            fingerprint: new_fingerprint().unwrap(),
            public_key: key.public_key().clone(),
            name: name.to_owned(),
            os: "linux".to_owned(),
            parent: None,
        };
        let fingerprint = self.reg.register_node(&node).unwrap().fingerprint;
        TestNode { fingerprint, key }
    }

    pub(super) fn session(&mut self) -> Fingerprint {
        self.reg.create_session(nerve::protocol::Agent::ClaudeCode).unwrap()
    }
}

pub(super) fn ghost() -> Fingerprint {
    Fingerprint::from_str("ghost").unwrap()
}
