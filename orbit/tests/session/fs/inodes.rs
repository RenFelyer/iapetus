use super::*;

impl Inodes {
    pub(crate) fn local_count(&self) -> usize {
        self.nodes
            .values()
            .filter(|node| node.local.is_some() && node.path.is_some())
            .count()
    }

    #[track_caller]
    pub(crate) fn verify_invariants(&self) {
        for (path, ino) in &self.by_path {
            let node = self.nodes.get(ino);
            assert!(node.is_some(), "путь {path:?} указывает на несуществующий inode {ino}");
            assert_eq!(
                node.and_then(|n| n.path.as_deref()),
                Some(path.as_str()),
                "путь {path:?} и путь записи inode {ino} разошлись"
            );
        }
        for (ino, node) in &self.nodes {
            if let Some(path) = &node.path {
                assert_eq!(
                    self.by_path.get(path),
                    Some(ino),
                    "inode {ino} назван путём {path:?}, но путь ведёт к другому inode"
                );
            }
        }
        assert!(self.nodes.contains_key(&ROOT), "корня нет в таблице");
    }
}
