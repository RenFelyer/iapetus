//! Кэш содержимого против эталонной модели: версия, которую видел агент, и копии в `/work` при обращениях, переносах,
//! простое и очистке. Время стоит и идёт только операцией простоя.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nerve::protocol::{Attr, Kind};
use proptest::prelude::*;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::time::Instant;

use super::{ContentCache, MAX_IDLE, Seen};

const FILES: [&str; 3] = ["a.rs", "d/b", "d/e/c.txt"];
const ATTR: Attr = Attr {
    kind: Kind::File,
    size: 0,
    mtime_ns: 0,
    mode: 0o644,
};

#[derive(Debug, Clone, proptest_derive::Arbitrary)]
enum Op {
    /// Агент прочитал файл: копия в `/work` и версия.
    Read {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
        #[proptest(strategy = "small_bytes()")]
        content: Vec<u8>,
    },
    /// Агент узнал, что файла нет.
    Absent {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
    },
    /// Агент открыл файл и держит его: копия обновляет время обращения и не удаляется, пока открыта.
    Open {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
    },
    /// Агент закрыл все открытые копии файла.
    Close {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
    },
    /// Файл перенесли на runner; под `to` теперь версия с содержимым `content`.
    Rename {
        #[proptest(strategy = "0..FILES.len()")]
        from: usize,
        #[proptest(strategy = "0..FILES.len()")]
        to: usize,
        #[proptest(strategy = "small_bytes()")]
        content: Vec<u8>,
    },
    /// Сессия забыла путь: агент удалил файл.
    Forget {
        #[proptest(strategy = "0..FILES.len()")]
        file: usize,
    },
    /// Проходит время, затем очистка.
    Idle {
        #[proptest(strategy = "0u64..180")]
        minutes: u64,
    },
}

fn small_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..5)
}

fn sha(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Копия в модели: поколение отличает её от прежних копий того же пути, которые ещё держат открытыми.
struct Copy {
    generation: u64,
    last_used: Instant,
}

struct Harness {
    _temp: TempDir,
    work: PathBuf,
    cache: ContentCache,
    /// Версия, на которую агент рассчитывает при записи; её не трогает никакая очистка.
    seen: BTreeMap<String, Seen>,
    copies: BTreeMap<String, Copy>,
    next_generation: u64,
    /// Открытые копии: путь, поколение и дескриптор.
    open: Vec<(String, u64, Arc<File>)>,
}

impl Harness {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        Self {
            cache: ContentCache::new(work.clone(), "SHA256:runner".to_owned(), PathBuf::from("/mnt/runner")),
            _temp: temp,
            work,
            seen: BTreeMap::new(),
            copies: BTreeMap::new(),
            next_generation: 0,
            open: Vec::new(),
        }
    }

    fn new_copy(&mut self) -> Copy {
        self.next_generation += 1;
        Copy {
            generation: self.next_generation,
            last_used: Instant::now(),
        }
    }

    async fn apply(&mut self, op: &Op) {
        match op {
            Op::Read { file, content } => {
                let path = FILES[*file];
                self.cache.store(path, Seen::Hash(sha(content)), Some(content), ATTR, 0).unwrap();
                self.seen.insert(path.to_owned(), Seen::Hash(sha(content)));
                let copy = self.new_copy();
                self.copies.insert(path.to_owned(), copy);
            }
            Op::Absent { file } => {
                let path = FILES[*file];
                self.cache.mark_absent(path, 0);
                self.seen.insert(path.to_owned(), Seen::Absent);
                self.copies.remove(path);
            }
            Op::Open { file } => {
                let path = FILES[*file];
                self.cache.touch(path);
                if let Some(copy) = self.copies.get_mut(path) {
                    copy.last_used = Instant::now();
                    let handle = self.cache.get(path).and_then(|entry| entry.file.clone()).unwrap();
                    self.open.push((path.to_owned(), copy.generation, handle));
                }
            }
            Op::Close { file } => self.open.retain(|(path, _, _)| path != FILES[*file]),
            Op::Rename { from, to, content } => {
                let (from, to) = (FILES[*from], FILES[*to]);
                // Перенос на то же имя ядро обрабатывает само и в ФС не присылает.
                if from == to {
                    return;
                }
                let hash = sha(content);
                self.cache.rename(from, to, hash, ATTR, 0);
                // Копия переезжает, только если под `from` была именно эта версия.
                let moved = if self.seen.get(from) == Some(&Seen::Hash(hash)) {
                    self.copies.remove(from)
                } else {
                    None
                };
                self.copies.remove(from);
                self.copies.remove(to);
                self.seen.remove(from);
                self.seen.insert(to.to_owned(), Seen::Hash(hash));
                if let Some(mut copy) = moved {
                    copy.last_used = Instant::now();
                    for (path, generation, _) in &mut self.open {
                        if *generation == copy.generation {
                            *path = to.to_owned();
                        }
                    }
                    self.copies.insert(to.to_owned(), copy);
                }
            }
            Op::Forget { file } => {
                let path = FILES[*file];
                self.cache.forget(path);
                self.seen.remove(path);
                self.copies.remove(path);
            }
            Op::Idle { minutes } => {
                tokio::time::advance(Duration::from_mins(*minutes)).await;
                let removed = self.cache.sweep();
                let held = |path: &str, copy: &Copy| {
                    self.open
                        .iter()
                        .any(|(open, generation, _)| open == path && *generation == copy.generation)
                };
                let expired: Vec<String> = self
                    .copies
                    .iter()
                    .filter(|(path, copy)| copy.last_used.elapsed() > MAX_IDLE && !held(path, copy))
                    .map(|(path, _)| path.clone())
                    .collect();
                for path in &expired {
                    self.copies.remove(path);
                }
                assert_eq!(
                    removed,
                    expired.len(),
                    "очистка после {minutes} мин простоя удалила не те копии, что простаивали дольше двух часов закрытыми"
                );
            }
        }
    }

    #[track_caller]
    fn verify_invariants(&self) {
        self.cache.verify_invariants();
        for path in FILES {
            let entry = self.cache.get(path);
            assert_eq!(
                entry.map(|entry| entry.seen),
                self.seen.get(path).copied(),
                "версия {path}, которую видел агент, расходится с моделью: запись потеряна или лишняя"
            );
            assert_eq!(
                entry.is_some_and(|entry| entry.file.is_some()),
                self.copies.contains_key(path),
                "копия {path} есть в кэше, но не в модели, или наоборот"
            );
        }
        let on_disk = std::fs::read_dir(&self.work).unwrap().count();
        assert_eq!(
            on_disk,
            self.copies.len(),
            "в /work файлов не столько, сколько копий: файл потерян или забыт"
        );
    }
}

fn check_ops(ops: &[Op]) {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap()
        .block_on(async {
            let mut harness = Harness::new();
            for op in ops {
                harness.apply(op).await;
                harness.verify_invariants();
            }
        });
}

#[test]
fn cache_matches_the_model_on_random_operations() {
    if std::env::var_os("RUN_SLOW_TESTS").is_none() {
        return;
    }
    proptest!(|(ops in proptest::collection::vec(any::<Op>(), 1..40))| check_ops(&ops));
}

impl ContentCache {
    #[track_caller]
    pub(crate) fn verify_invariants(&self) {
        use std::os::unix::fs::FileExt;
        for (path, entry) in &self.entries {
            let (Some(file), Seen::Hash(hash)) = (&entry.file, entry.seen) else {
                continue;
            };
            let len = file.metadata().unwrap().len();
            let mut bytes = vec![0; usize::try_from(len).unwrap()];
            file.read_exact_at(&mut bytes, 0).unwrap();
            let on_disk: [u8; 32] = Sha256::digest(&bytes).into();
            assert_eq!(on_disk, hash, "кэш-файл {path:?} не соответствует версии, которую видел агент");
        }
    }
}
