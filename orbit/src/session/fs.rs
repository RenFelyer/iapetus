//! FUSE orbit session: рабочий каталог агента — файловая система сессии, содержимое которой берётся с runner.
//!
//! Ядро [`Vfs`] хранит состояние и принимает решения без fuser, [`mount`] подключает его к ядру Linux. У проекта и у
//! `$HOME` runner свой [`Vfs`]. Кэш метаданных обновляют события канала данных ([`follow_watch`]); «новое обращение»
//! к файлу начинается со смены эпохи ([`Epoch::advance`]).

mod adapter;
mod cache;
mod inodes;
mod meta;
mod remote;
mod vfs;
mod watch;

#[cfg(test)]
#[path = "../../tests/session/fs/model.rs"]
mod model;

pub(super) use adapter::mount;
pub(super) use inodes::Owner;
pub(super) use vfs::{Config, Epoch, Profile, Symlink, Vfs, preload};
pub(super) use watch::follow as follow_watch;
