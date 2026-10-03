use std::os::unix::fs::MetadataExt as _;

use tempfile::TempDir;
use tokio::io::AsyncWriteExt as _;

use super::*;
use crate::session::shell::protocol::ExitStatus;

/// Владелец сокета, отличный от создателя: иначе проверка владельца ничего не заметила бы. root отдаёт сокет
/// любому uid; остальные могут отдать его только своей группе, поэтому для них проверяется то, что возможно.
fn other_owner(dir: &TempDir) -> Owner {
    let own = std::fs::metadata(dir.path()).unwrap();
    if own.uid() == 0 {
        return Owner { uid: 12345, gid: 12345 };
    }
    Owner {
        uid: own.uid(),
        gid: own.gid(),
    }
}

#[tokio::test]
async fn socket_is_private_to_the_owner() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("run").join("shell.sock");
    let owner = other_owner(&dir);
    // Остаток прошлого запуска на том же пути не мешает.
    // Сокет создаётся от имени владельца: ему нужен доступ к каталогу, как у /run/iapetus в образе session.
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    for path in [dir.path(), socket.parent().unwrap()] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777)).unwrap();
    }
    std::fs::write(&socket, "stale").unwrap();
    let _listener = bind(&socket, owner).unwrap();

    let metadata = std::fs::metadata(&socket).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600, "сокет доступен не только владельцу");
    assert_eq!((metadata.uid(), metadata.gid()), (owner.uid, owner.gid), "сокет отдан не владельцу");
}

#[tokio::test]
async fn garbage_on_the_socket_does_not_stop_the_agent() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("shell.sock");
    let own = std::fs::metadata(dir.path()).unwrap();
    let listener = bind(
        &socket,
        Owner {
            uid: own.uid(),
            gid: own.gid(),
        },
    )
    .unwrap();
    let (tasks, mut received) = mpsc::channel(8);
    tokio::spawn(serve(listener, tasks));

    let garbage: [&[u8]; 3] = [&[0xff; 4], &[0, 0, 0, 3, 9, 9, 9], &[0, 0, 0, 200, 1]];
    for bytes in garbage {
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        stream.write_all(bytes).await.unwrap();
    }

    let mut stream = UnixStream::connect(&socket).await.unwrap();
    let command = ClientMessage::Run {
        command: "echo still-works".to_owned(),
        cwd: "/x".to_owned(),
    };
    write_frame(&mut stream, &command).await.unwrap();
    let task = received.recv().await.expect("агент не передал команду после мусора на сокете");
    assert_eq!(task.command, "echo still-works");
    let exit = Reply::Exit {
        status: ExitStatus::Code(0),
        cwd: "/x".to_owned(),
    };
    task.reply.send(exit.clone()).await.unwrap();
    let reply: Reply = read_frame(&mut stream).await.unwrap();
    assert_eq!(reply, exit, "оболочка не получила ответ после мусора на сокете");
}
