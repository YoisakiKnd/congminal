//! Windows 原生运行测试；交叉 cargo check 只验证编译，不记作运行通过。
use super::*;
use crate::ssh::auth::{
    Front, Owner,
    ticket::{Ticket as AuthTicket, windows::Stream},
};
use std::path::PathBuf;
struct Folder(PathBuf);
impl Folder {
    fn new() -> Self {
        let random = wire::Capability::generate().unwrap();
        let path = std::env::temp_dir().join(format!(
            "cg-auth-test-{}-{:016x}",
            std::process::id(),
            u64::from_le_bytes(random.bytes()[..8].try_into().unwrap())
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Folder {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
fn setup(folder: &Folder, pool: &Arc<Budget>) -> (Pump, Front, Cancellation) {
    let (back, front) = Backend::new(Owner { session: 7, generation: 3 }, pool, Arc::new(|| {})).unwrap();
    let cancel = Cancellation::default();
    (Pump::new(&folder.0, back, pool, &cancel).unwrap(), front, cancel)
}
fn until(mut action: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !action() {
        assert!(Instant::now() < deadline, "bounded pipe operation timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn peer(pump: &Pump, pool: &Arc<Budget>, cancel: &Cancellation, kind: Kind) -> (Stream, wire::Packet) {
    let ticket = AuthTicket::read_private(pump.ticket_path()).unwrap();
    let packet = wire::request(&ticket.capability, kind, b"private prompt", pool, cancel).unwrap();
    (Stream::connect(pump.ticket_path()).unwrap(), packet)
}
#[test]
fn windows_private_pipe_moves_maximum_answer_waits_for_ack_and_releases_all_credit() {
    let folder = Folder::new();
    let pool = Budget::new(512 * 1024, 0);
    let (mut pump, front, cancel) = setup(&folder, &pool);
    let directory = pump.ticket_path().parent().unwrap().to_owned();
    let (mut client, packet) = peer(&pump, &pool, &cancel, Kind::Secret);
    client.write_all(packet.bytes()).unwrap();
    drop(packet);
    until(|| {
        pump.tick(&cancel).unwrap();
        front.snapshot().input.is_some()
    });
    let snapshot = front.snapshot();
    let ticket = snapshot.input.as_ref().unwrap().ticket();
    let mut secret = front.input(ticket).unwrap();
    secret.insert(0, &"x".repeat(1020)).unwrap();
    secret.insert(1020, "文").unwrap();
    front.answer(ticket, secret).unwrap();
    let mut decoder = wire::AnswerDecoder::new(pool.clone());
    let mut answer = None;
    until(|| {
        pump.tick(&cancel).unwrap();
        let mut bytes = [0u8; 97];
        match client.read(&mut bytes) {
            Ok(n) if n > 0 => {
                answer = decoder.push(&bytes[..n], &cancel).unwrap();
                answer.is_some()
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
            other => panic!("pipe reply missing: {other:?}"),
        }
    });
    assert!(
        matches!(&answer, Some(wire::Answer::Accepted(s)) if s.len() == 1023 && s.bytes().ends_with("文".as_bytes()))
    );
    assert!(pump.clients.iter().any(Option::is_some), "server must keep queued answer alive until acknowledged");
    client.acknowledge().unwrap();
    until(|| {
        pump.tick(&cancel).unwrap();
        pump.clients.iter().all(Option::is_none)
    });
    drop((answer, decoder, client, snapshot));
    pump.finish().unwrap();
    assert!(!directory.exists());
    drop(front);
    assert_eq!(pool.snapshot().used, 0);
}
#[test]
fn windows_wrong_capability_extra_bytes_split_message_and_cancel_fail_closed() {
    for scenario in ["capability", "extra", "split", "cancel"] {
        let folder = Folder::new();
        let pool = Budget::new(512 * 1024, 0);
        let (mut pump, front, cancel) = setup(&folder, &pool);
        let (mut client, packet) = peer(&pump, &pool, &cancel, Kind::Secret);
        let mut bytes = packet.bytes().to_vec();
        match scenario {
            "capability" => {
                bytes[8] ^= 1;
                client.write_all(&bytes).unwrap();
            }
            "extra" => {
                bytes.extend_from_slice(b"extra");
                client.write_all(&bytes).unwrap();
            }
            "split" => {
                client.write_all(&bytes[..1]).unwrap();
            }
            "cancel" => cancel.cancel(),
            _ => unreachable!(),
        }
        until(|| pump.tick(&cancel).is_err());
        assert!(front.snapshot().input.is_none());
        drop((bytes, packet, client));
        pump.finish().unwrap();
        drop(front);
        assert_eq!(pool.snapshot().used, 0);
    }
}
#[test]
fn windows_public_ticket_is_rejected_and_cancelled_creation_spends_no_credit() {
    let folder = Folder::new();
    let pool = Budget::new(512 * 1024, 0);
    let (pump, front, cancel) = setup(&folder, &pool);
    let bytes = std::fs::read(pump.ticket_path()).unwrap();
    let fake_directory = folder.0.join(pump.ticket_path().parent().unwrap().file_name().unwrap());
    let exposed = folder.0.join("public");
    std::fs::create_dir(&exposed).unwrap();
    let exposed = exposed.join(fake_directory.file_name().unwrap());
    std::fs::create_dir(&exposed).unwrap();
    let exposed = exposed.join("ticket");
    std::fs::write(&exposed, bytes).unwrap();
    assert!(AuthTicket::read_private(&exposed).is_err());
    pump.finish().unwrap();
    drop(front);
    cancel.cancel();
    assert!(OwnedTicket::create(&folder.0, &pool, &cancel).is_err());
    let (back, front) = Backend::new(Owner { session: 8, generation: 1 }, &pool, Arc::new(|| {})).unwrap();
    let failed = Pump::new_checked(&folder.0, back, &pool, &cancel).err().unwrap();
    assert!(failed.cleanup_confirmed);
    drop(front);
    assert_eq!(pool.snapshot().used, 0);
}
