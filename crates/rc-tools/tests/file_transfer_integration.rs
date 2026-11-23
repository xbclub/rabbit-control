use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rc_tools::file_transfer::{
    create_local_dir, list_local_dir, FileClientEngine, FileEvent, FileHostEngine, FileWireMsg,
    TransferDirection, TransferStatus, MAX_FILE_WIRE_FRAME_BYTES,
};

static TEST_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let id = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "rc_tools_ft_it_{tag}_{}_{}",
            std::process::id(),
            id
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("create temp dir");
        Self(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn deterministic_payload(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((state >> 56) as u8);
    }
    out
}

fn find_rcpart(dir: &Path, _file_name: &str, id: u64) -> Option<PathBuf> {
    let prefix = format!(".rcpart.{id}.");
    fs::read_dir(dir).ok()?.find_map(|entry| {
        let entry = entry.ok()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) {
            Some(entry.path())
        } else {
            None
        }
    })
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn pump_all(
    client: &mut FileClientEngine,
    host: &mut FileHostEngine,
    batch_size: usize,
) -> Vec<FileEvent> {
    let mut events = Vec::new();
    for _ in 0..2000 {
        let c_frames = client.poll_outgoing(batch_size);
        let h_frames = host.poll_outgoing(batch_size);
        if c_frames.is_empty() && h_frames.is_empty() {
            break;
        }
        for f in c_frames {
            assert!(
                f.len() <= MAX_FILE_WIRE_FRAME_BYTES,
                "client frame exceeded 192 KiB: {}",
                f.len()
            );
            host.handle_incoming(&f);
        }
        for f in h_frames {
            assert!(
                f.len() <= MAX_FILE_WIRE_FRAME_BYTES,
                "host frame exceeded 192 KiB: {}",
                f.len()
            );
            events.extend(client.handle_incoming(&f));
        }
    }
    events
}

#[test]
fn remote_list_dir_and_mkdir_with_traversal_rejection() {
    let remote = TempDir::new("dir_ops");
    let remote_str = remote.path().to_str().unwrap().to_string();

    let mut client = FileClientEngine::new();
    let mut host = FileHostEngine::new();

    client.request_mkdir(remote_str.clone(), "alpha".to_string());
    client.request_mkdir(remote_str.clone(), "../escape".to_string());
    client.request_mkdir(remote_str.clone(), "bad/name".to_string());
    client.request_list_dir(Some(remote_str.clone()));

    let events = pump_all(&mut client, &mut host, 8);

    let mkdir_results: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            FileEvent::MkdirResult { path, ok, error } => Some((path, *ok, error)),
            _ => None,
        })
        .collect();
    assert_eq!(mkdir_results.len(), 3);
    assert!(mkdir_results[0].1);
    assert!(!mkdir_results[1].1 && mkdir_results[1].2.is_some());
    assert!(!mkdir_results[2].1 && mkdir_results[2].2.is_some());

    let dir_event = events
        .iter()
        .find_map(|e| match e {
            FileEvent::RemoteDir {
                path,
                entries,
                error,
            } => Some((path, entries, error)),
            _ => None,
        })
        .expect("RemoteDir event");
    assert!(dir_event.2.is_none());
    assert_eq!(dir_event.1.len(), 1);
    assert_eq!(dir_event.1[0].name, "alpha");
    assert!(dir_event.1[0].is_dir);

    // Local helpers double-check
    assert!(create_local_dir(&remote_str, "..").is_err());
    let (_p, local_entries) = list_local_dir(&remote_str).unwrap();
    assert_eq!(local_entries.len(), 1);
    assert_eq!(local_entries[0].name, "alpha");
}

#[test]
fn multi_chunk_upload_over_200kib_exact_content_match() {
    let local = TempDir::new("up_local");
    let remote = TempDir::new("up_remote");

    let payload = deterministic_payload(260 * 1024 + 137, 0xCAFE_BABE_1234_5678);
    let src_file = local.path().join("upload_sample.bin");
    fs::write(&src_file, &payload).unwrap();

    let mut client = FileClientEngine::new();
    let mut host = FileHostEngine::new();

    let id = client
        .start_upload(src_file.to_str().unwrap(), remote.path().to_str().unwrap())
        .unwrap();

    let events = pump_all(&mut client, &mut host, 2);
    assert!(events.iter().any(|e| matches!(
        e,
        FileEvent::TransferUpdated(item)
            if item.id == id
                && item.direction == TransferDirection::Upload
                && item.status == TransferStatus::Done
                && item.done_bytes == payload.len() as u64
    )));

    let dst_file = remote.path().join("upload_sample.bin");
    let received = fs::read(&dst_file).unwrap();
    assert_eq!(received.len(), payload.len());
    assert_eq!(fnv1a_64(&received), fnv1a_64(&payload));
    assert_eq!(received, payload);

    // Ensure no leftover .rcpart files in remote dir
    let remaining: Vec<_> = fs::read_dir(remote.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(remaining, vec!["upload_sample.bin"]);
    assert_eq!(host.active_upload_count(), 0);
}

#[test]
fn multi_chunk_download_over_200kib_exact_content_match() {
    let local = TempDir::new("dl_local");
    let remote = TempDir::new("dl_remote");

    let payload = deterministic_payload(275 * 1024 + 55, 0xDEAD_BEEF_9988_7766);
    let src_file = remote.path().join("remote_archive.bin");
    fs::write(&src_file, &payload).unwrap();

    let mut client = FileClientEngine::new();
    let mut host = FileHostEngine::new();

    let id = client
        .start_download(src_file.to_str().unwrap(), local.path().to_str().unwrap())
        .unwrap();

    let events = pump_all(&mut client, &mut host, 2);
    assert!(events.iter().any(|e| matches!(
        e,
        FileEvent::TransferUpdated(item)
            if item.id == id
                && item.direction == TransferDirection::Download
                && item.status == TransferStatus::Done
                && item.done_bytes == payload.len() as u64
    )));

    let dst_file = local.path().join("remote_archive.bin");
    let received = fs::read(&dst_file).unwrap();
    assert_eq!(received.len(), payload.len());
    assert_eq!(fnv1a_64(&received), fnv1a_64(&payload));
    assert_eq!(received, payload);

    let remaining: Vec<_> = fs::read_dir(local.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(remaining, vec!["remote_archive.bin"]);
    assert_eq!(host.active_download_count(), 0);
}

#[test]
fn mid_transfer_cancel_cleans_up_rcpart_file() {
    let local = TempDir::new("cancel_local");
    let remote = TempDir::new("cancel_remote");

    let payload = deterministic_payload(256 * 1024, 0x1122_3344_5566_7788);
    let src_file = local.path().join("big.dat");
    fs::write(&src_file, &payload).unwrap();

    let mut client = FileClientEngine::new();
    let mut host = FileHostEngine::new();

    let id = client
        .start_upload(src_file.to_str().unwrap(), remote.path().to_str().unwrap())
        .unwrap();

    // 1. Send UploadStart -> Host
    let start_frames = client.poll_outgoing(1);
    assert_eq!(start_frames.len(), 1);
    host.handle_incoming(&start_frames[0]);

    // 2. Receive UploadAck -> Client
    let ack_frames = host.poll_outgoing(1);
    assert_eq!(ack_frames.len(), 1);
    client.handle_incoming(&ack_frames[0]);

    // 3. Send just 1 chunk (64 KiB of 256 KiB)
    let first_chunk = client.poll_outgoing(1);
    assert_eq!(first_chunk.len(), 1);
    host.handle_incoming(&first_chunk[0]);

    // Verify .rcpart temp file exists mid-transfer
    let part_path =
        find_rcpart(remote.path(), "big.dat", id).expect("expected .rcpart temp file mid-transfer");

    // 4. Cancel mid-transfer and pump cancel frame to host
    client.cancel_transfer(id);
    let _ = pump_all(&mut client, &mut host, 8);

    assert!(
        !part_path.exists() && find_rcpart(remote.path(), "big.dat", id).is_none(),
        ".rcpart temp file must be removed after cancel"
    );
    assert!(!remote.path().join("big.dat").exists());
    assert_eq!(host.active_upload_count(), 0);

    let list = client.transfers();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].status, TransferStatus::Cancelled);

    client.clear_finished();
    assert!(client.transfers().is_empty());
}

#[test]
fn host_rejects_upload_path_traversal_and_drop_cleans_rcpart() {
    let remote = TempDir::new("host_sec");
    let part_path;
    {
        let mut host = FileHostEngine::new();

        // Path traversal attempt via raw wire message
        let bad_start = FileWireMsg::UploadStart {
            id: 99,
            remote_dir: remote.path().to_str().unwrap().to_string(),
            file_name: "../pwned.txt".to_string(),
            total_bytes: 10,
        }
        .encode()
        .unwrap();
        host.handle_incoming(&bad_start);
        let resp_frames = host.poll_outgoing(4);
        assert_eq!(resp_frames.len(), 1);
        match FileWireMsg::decode(&resp_frames[0]).unwrap() {
            FileWireMsg::UploadAck { id, ok, error } => {
                assert_eq!(id, 99);
                assert!(!ok);
                assert!(error.is_some());
            }
            other => panic!("unexpected msg: {other:?}"),
        }

        // Valid upload left unfinished on Drop
        let good_start = FileWireMsg::UploadStart {
            id: 100,
            remote_dir: remote.path().to_str().unwrap().to_string(),
            file_name: "unfinished.bin".to_string(),
            total_bytes: 1024,
        }
        .encode()
        .unwrap();
        host.handle_incoming(&good_start);
        let _ = host.poll_outgoing(4);
        part_path = find_rcpart(remote.path(), "unfinished.bin", 100)
            .expect("expected unfinished .rcpart temp file");
        assert!(part_path.exists());
    }
    assert!(
        !part_path.exists(),
        "FileHostEngine::drop must clean up unfinished .rcpart files"
    );
}

#[test]
fn empty_file_roundtrip_and_mid_download_cancel_cleanup() {
    let local = TempDir::new("empty_local");
    let remote = TempDir::new("empty_remote");

    let empty_src = local.path().join("zero.txt");
    fs::write(&empty_src, b"").unwrap();

    let mut client = FileClientEngine::new();
    let mut host = FileHostEngine::new();

    // 0-byte upload
    let up_id = client
        .start_upload(empty_src.to_str().unwrap(), remote.path().to_str().unwrap())
        .unwrap();
    let _ = pump_all(&mut client, &mut host, 4);
    assert_eq!(fs::read(remote.path().join("zero.txt")).unwrap(), b"");
    assert_eq!(
        client
            .transfers()
            .into_iter()
            .find(|t| t.id == up_id)
            .unwrap()
            .status,
        TransferStatus::Done
    );

    // Mid-download cancel cleans up client's .rcpart
    let big_remote = remote.path().join("dl_big.bin");
    fs::write(&big_remote, deterministic_payload(256 * 1024, 42)).unwrap();

    let dl_id = client
        .start_download(big_remote.to_str().unwrap(), local.path().to_str().unwrap())
        .unwrap();

    // Send DownloadStart
    let req = client.poll_outgoing(1);
    host.handle_incoming(&req[0]);
    // Receive DownloadMeta + 1 DownloadChunk
    let first_two = host.poll_outgoing(2);
    assert_eq!(first_two.len(), 2);
    for f in first_two {
        let _ = client.handle_incoming(&f);
    }

    let local_part = find_rcpart(local.path(), "dl_big.bin", dl_id)
        .expect("expected client .rcpart mid-download");

    // remove_transfer cancels active download & removes record
    client.remove_transfer(dl_id);
    let _ = pump_all(&mut client, &mut host, 8);
    assert!(!local_part.exists() && find_rcpart(local.path(), "dl_big.bin", dl_id).is_none());
    assert_eq!(host.active_download_count(), 0);
    assert!(!client.transfers().iter().any(|t| t.id == dl_id));
}

#[test]
fn malicious_download_meta_file_name_is_rejected() {
    let local = TempDir::new("h8_local");
    let mut client = FileClientEngine::new();

    let dl_id = client
        .start_download("/remote/docs/readme.txt", local.path().to_str().unwrap())
        .unwrap();
    let _ = client.poll_outgoing(4); // drain DownloadStart

    // Malicious host tries to overwrite `.zshrc` instead of `readme.txt`.
    let evil_meta = FileWireMsg::DownloadMeta {
        id: dl_id,
        file_name: ".zshrc".to_string(),
        total_bytes: 32,
        ok: true,
        error: None,
    }
    .encode()
    .unwrap();

    let events = client.handle_incoming(&evil_meta);
    assert!(events.iter().any(|e| matches!(
        e,
        FileEvent::TransferUpdated(item)
            if item.id == dl_id && item.status == TransferStatus::Failed && item.error.is_some()
    )));
    assert!(!local.path().join(".zshrc").exists());
    assert!(find_rcpart(local.path(), ".zshrc", dl_id).is_none());

    let out: Vec<FileWireMsg> = client
        .poll_outgoing(4)
        .into_iter()
        .map(|f| FileWireMsg::decode(&f).unwrap())
        .collect();
    assert!(out
        .iter()
        .any(|m| matches!(m, FileWireMsg::CancelTransfer { id } if *id == dl_id)));
}

#[test]
fn duplicate_download_meta_is_rejected_without_leaking_rcpart() {
    let local = TempDir::new("h9_local");
    let mut client = FileClientEngine::new();

    let dl_id = client
        .start_download("/remote/report.bin", local.path().to_str().unwrap())
        .unwrap();
    let _ = client.poll_outgoing(4);

    let meta = FileWireMsg::DownloadMeta {
        id: dl_id,
        file_name: "report.bin".to_string(),
        total_bytes: 128,
        ok: true,
        error: None,
    }
    .encode()
    .unwrap();

    // First DownloadMeta creates .rcpart
    let ev1 = client.handle_incoming(&meta);
    assert_eq!(ev1.len(), 1);
    let part = find_rcpart(local.path(), "report.bin", dl_id)
        .expect("first DownloadMeta should create .rcpart");
    assert!(part.exists());

    // Duplicate DownloadMeta must fail transfer, remove .rcpart, and send CancelTransfer
    let ev2 = client.handle_incoming(&meta);
    assert!(ev2.iter().any(|e| matches!(
        e,
        FileEvent::TransferUpdated(item)
            if item.id == dl_id && item.status == TransferStatus::Failed
    )));
    assert!(
        !part.exists() && find_rcpart(local.path(), "report.bin", dl_id).is_none(),
        "duplicate DownloadMeta leaked .rcpart file"
    );

    let out: Vec<FileWireMsg> = client
        .poll_outgoing(4)
        .into_iter()
        .map(|f| FileWireMsg::decode(&f).unwrap())
        .collect();
    assert!(out
        .iter()
        .any(|m| matches!(m, FileWireMsg::CancelTransfer { id } if *id == dl_id)));
}

#[test]
fn long_220_byte_filename_upload_and_download_succeed_without_enametoolong() {
    let local = TempDir::new("long_local");
    let remote = TempDir::new("long_remote");
    let dl_dir = TempDir::new("long_dl");

    let long_name = format!("{}.bin", "a".repeat(216));
    assert_eq!(long_name.len(), 220);

    let payload = deterministic_payload(80 * 1024 + 13, 0x55AA_1234_9876_FEDC);
    let src_file = local.path().join(&long_name);
    fs::write(&src_file, &payload).unwrap();

    let mut client = FileClientEngine::new();
    let mut host = FileHostEngine::new();

    // Upload 220-byte filename
    let up_id = client
        .start_upload(src_file.to_str().unwrap(), remote.path().to_str().unwrap())
        .unwrap();
    let _ = pump_all(&mut client, &mut host, 4);
    assert_eq!(
        client
            .transfers()
            .into_iter()
            .find(|t| t.id == up_id)
            .unwrap()
            .status,
        TransferStatus::Done
    );
    let remote_file = remote.path().join(&long_name);
    assert_eq!(fs::read(&remote_file).unwrap(), payload);

    // Download 220-byte filename
    let dl_id = client
        .start_download(
            remote_file.to_str().unwrap(),
            dl_dir.path().to_str().unwrap(),
        )
        .unwrap();
    let _ = pump_all(&mut client, &mut host, 4);
    assert_eq!(
        client
            .transfers()
            .into_iter()
            .find(|t| t.id == dl_id)
            .unwrap()
            .status,
        TransferStatus::Done
    );
    assert_eq!(fs::read(dl_dir.path().join(&long_name)).unwrap(), payload);
}

#[test]
fn upload_done_for_download_id_is_ignored() {
    let local = TempDir::new("m6_local");
    let remote = TempDir::new("m6_remote");

    let payload = deterministic_payload(128 * 1024, 0xCAFE_1234_ABCD_5678);
    let remote_file = remote.path().join("legit.bin");
    fs::write(&remote_file, &payload).unwrap();

    let mut client = FileClientEngine::new();
    let mut host = FileHostEngine::new();

    let dl_id = client
        .start_download(
            remote_file.to_str().unwrap(),
            local.path().to_str().unwrap(),
        )
        .unwrap();

    // Inject spurious UploadDone & UploadAck targeting the active download ID
    let fake_ack = FileWireMsg::UploadAck {
        id: dl_id,
        ok: false,
        error: Some("spoofed ack".to_string()),
    }
    .encode()
    .unwrap();
    let fake_done = FileWireMsg::UploadDone {
        id: dl_id,
        ok: true,
        error: None,
    }
    .encode()
    .unwrap();

    assert!(client.handle_incoming(&fake_ack).is_empty());
    assert!(client.handle_incoming(&fake_done).is_empty());

    let item = client
        .transfers()
        .into_iter()
        .find(|t| t.id == dl_id)
        .unwrap();
    assert_eq!(item.status, TransferStatus::Active);
    assert_eq!(item.direction, TransferDirection::Download);

    // Genuine download still completes intact
    let _ = pump_all(&mut client, &mut host, 4);
    assert_eq!(fs::read(local.path().join("legit.bin")).unwrap(), payload);
}

#[cfg(unix)]
#[test]
fn symlink_download_uses_requested_basename_and_caps_growing_file() {
    use std::os::unix::fs::symlink;

    let local = TempDir::new("sym_local");
    let remote = TempDir::new("sym_remote");

    let target = remote.path().join("real_target.dat");
    let initial_data = deterministic_payload(128 * 1024, 0x1020_3040_5060_7080);
    fs::write(&target, &initial_data).unwrap();

    let link = remote.path().join("public_alias.dat");
    symlink(&target, &link).unwrap();

    let mut client = FileClientEngine::new();
    let mut host = FileHostEngine::new();

    let dl_id = client
        .start_download(link.to_str().unwrap(), local.path().to_str().unwrap())
        .unwrap();

    // Send DownloadStart -> Host
    let req = client.poll_outgoing(1);
    host.handle_incoming(&req[0]);

    // Receive DownloadMeta + first chunk
    let first = host.poll_outgoing(2);
    for f in first {
        let _ = client.handle_incoming(&f);
    }

    // Append extra bytes to the underlying file mid-download; host must cap to declared total_bytes
    let mut f = fs::OpenOptions::new().append(true).open(&target).unwrap();
    use std::io::Write;
    f.write_all(&[0xFF; 16 * 1024]).unwrap();
    drop(f);

    let _ = pump_all(&mut client, &mut host, 4);
    let item = client
        .transfers()
        .into_iter()
        .find(|t| t.id == dl_id)
        .unwrap();
    assert_eq!(item.status, TransferStatus::Done);
    assert_eq!(item.name, "public_alias.dat");
    assert_eq!(
        fs::read(local.path().join("public_alias.dat")).unwrap(),
        initial_data
    );
    assert!(!local.path().join("real_target.dat").exists());
}
