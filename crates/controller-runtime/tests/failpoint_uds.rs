#![cfg(feature = "test-failpoints")]

use std::{
    ffi::OsString,
    fs,
    io::Write,
    os::unix::{
        ffi::OsStringExt,
        fs::{FileTypeExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use oxideloop_controller_runtime::{
    FailpointV0, FailpointsV0,
    failpoint_uds::{FailpointUdsError, FailpointUdsListenerV0, MAX_REQUEST_BYTES},
};

struct TestDir(PathBuf);

static TEST_DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

impl TestDir {
    fn new() -> Self {
        let unique = format!(
            "oxideloop-failpoint-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            TEST_DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed),
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir(&path).expect("test directory");
        let mut permissions = fs::metadata(&path).expect("test metadata").permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).expect("test directory permissions");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn socket_path(root: &TestDir, name: &str) -> PathBuf {
    root.path().join(name)
}

fn connect(path: &Path) -> UnixStream {
    UnixStream::connect(path).expect("connect failpoint socket")
}

fn send(path: &Path, request: &[u8]) {
    let mut stream = connect(path);
    stream.write_all(request).expect("write failpoint request");
}

fn request_error(request: &[u8]) -> FailpointUdsError {
    let root = TestDir::new();
    let path = socket_path(&root, "failpoint.sock");
    let mut listener = FailpointUdsListenerV0::bind(&path).expect("bind listener");
    send(&path, request);
    let error = listener.poll().expect_err("request rejected");
    assert!(listener.is_consumed());
    assert!(
        !path.exists(),
        "rejected request cleans up the one-shot socket"
    );
    error
}

#[test]
fn creates_private_parent_and_socket_and_cleans_up_after_arm() {
    let root = TestDir::new();
    let parent = root.path().join("private");
    let path = parent.join("failpoint.sock");
    let mut listener = FailpointUdsListenerV0::bind(&path).expect("bind listener");

    assert_eq!(
        fs::metadata(&parent).expect("parent").permissions().mode() & 0o7777,
        0o700
    );
    let socket = fs::symlink_metadata(&path).expect("socket");
    assert!(socket.file_type().is_socket());
    assert_eq!(socket.permissions().mode() & 0o7777, 0o600);

    send(&path, b"AfterEventBeforeStatus\n");
    assert_eq!(
        listener.poll().expect("poll"),
        Some(FailpointV0::AfterEventBeforeStatus)
    );
    assert!(
        !path.exists(),
        "consuming the one-shot arm removes the socket"
    );
}

#[test]
fn dropping_an_unconsumed_listener_cleans_up_its_socket() {
    let root = TestDir::new();
    let path = socket_path(&root, "failpoint.sock");
    let listener = FailpointUdsListenerV0::bind(&path).expect("bind listener");
    assert!(path.exists());
    drop(listener);
    assert!(!path.exists());
}

#[test]
fn accepts_exact_values_and_rejects_empty_nul_trailing_and_oversized_requests() {
    for (request, expected) in [
        (
            b"AfterEventBeforeStatus\n".as_slice(),
            FailpointV0::AfterEventBeforeStatus,
        ),
        (
            b"AfterStatusBeforeAck\n".as_slice(),
            FailpointV0::AfterStatusBeforeAck,
        ),
    ] {
        let root = TestDir::new();
        let path = socket_path(&root, "failpoint.sock");
        let mut listener = FailpointUdsListenerV0::bind(&path).expect("bind listener");
        send(&path, request);
        assert_eq!(listener.poll().expect("accepted request"), Some(expected));
    }

    assert!(matches!(
        request_error(b"\n"),
        FailpointUdsError::MalformedRequest
    ));
    assert!(matches!(
        request_error(b"AfterEvent\0BeforeStatus\n"),
        FailpointUdsError::MalformedRequest
    ));
    assert!(matches!(
        request_error(b"AfterEventBeforeStatus\ntrailing"),
        FailpointUdsError::MalformedRequest
    ));
    assert!(matches!(
        request_error(&[b'x'; MAX_REQUEST_BYTES]),
        FailpointUdsError::OversizeRequest
    ));
}

#[test]
fn slow_request_has_one_total_deadline() {
    let root = TestDir::new();
    let path = socket_path(&root, "failpoint.sock");
    let mut listener = FailpointUdsListenerV0::bind(&path).expect("bind listener");
    let mut client = connect(&path);
    client.write_all(b"A").expect("initial byte");
    let writer = thread::spawn(move || {
        for byte in b"fterEventBeforeStatus\n" {
            thread::sleep(Duration::from_millis(20));
            if client.write_all(&[*byte]).is_err() {
                return;
            }
        }
    });

    assert!(matches!(
        listener.poll(),
        Err(FailpointUdsError::RequestDeadlineExceeded)
    ));
    assert!(listener.is_consumed());
    writer.join().expect("writer thread");
}

#[test]
fn arm_into_is_one_shot_for_both_automation_windows() {
    for (index, (request, expected)) in [
        (
            b"AfterEventBeforeStatus\n".as_slice(),
            FailpointV0::AfterEventBeforeStatus,
        ),
        (
            b"AfterStatusBeforeAck\n".as_slice(),
            FailpointV0::AfterStatusBeforeAck,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let root = TestDir::new();
        let path = socket_path(&root, &format!("failpoint-{index}.sock"));
        let mut listener = FailpointUdsListenerV0::bind(&path).expect("bind listener");
        let mut state = FailpointsV0::default();
        send(&path, request);
        assert_eq!(listener.arm_into(&mut state).expect("arm"), Some(expected));
        assert!(listener.is_consumed());
        assert!(state.take(expected));
        assert!(!state.take(expected));
        assert!(matches!(
            listener.poll(),
            Err(FailpointUdsError::AlreadyConsumed)
        ));
    }
}

#[test]
fn rejects_empty_nul_and_trailing_socket_paths() {
    assert!(matches!(
        FailpointUdsListenerV0::bind(PathBuf::new()),
        Err(FailpointUdsError::InvalidPath)
    ));
    let nul_path = PathBuf::from(OsString::from_vec(b"/tmp/failpoint\0socket".to_vec()));
    assert!(matches!(
        FailpointUdsListenerV0::bind(nul_path),
        Err(FailpointUdsError::InvalidPath)
    ));
    assert!(matches!(
        FailpointUdsListenerV0::bind(PathBuf::from("/tmp/failpoint/")),
        Err(FailpointUdsError::InvalidPath)
    ));
}

#[test]
fn refuses_existing_paths_symlinks_non_private_parents_and_mutable_ancestors() {
    let root = TestDir::new();
    let existing = socket_path(&root, "existing");
    fs::write(&existing, b"occupied").expect("existing path");
    assert!(matches!(
        FailpointUdsListenerV0::bind(&existing),
        Err(FailpointUdsError::ExistingPath)
    ));
    assert_eq!(
        fs::read(&existing).expect("existing preserved"),
        b"occupied"
    );

    let target = socket_path(&root, "target");
    fs::write(&target, b"target").expect("symlink target");
    let link = socket_path(&root, "link");
    std::os::unix::fs::symlink(&target, &link).expect("socket symlink");
    assert!(matches!(
        FailpointUdsListenerV0::bind(&link),
        Err(FailpointUdsError::ExistingPath)
    ));

    let private = root.path().join("private");
    fs::create_dir(&private).expect("private parent");
    let mut permissions = fs::metadata(&private)
        .expect("private metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&private, permissions).expect("parent permissions");
    assert!(matches!(
        FailpointUdsListenerV0::bind(private.join("failpoint.sock")),
        Err(FailpointUdsError::ParentNotPrivate)
    ));

    let real_parent = root.path().join("real-parent");
    fs::create_dir(&real_parent).expect("real parent");
    let mut permissions = fs::metadata(&real_parent)
        .expect("real metadata")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&real_parent, permissions).expect("real permissions");
    let parent_link = root.path().join("parent-link");
    std::os::unix::fs::symlink(&real_parent, &parent_link).expect("parent symlink");
    assert!(matches!(
        FailpointUdsListenerV0::bind(parent_link.join("failpoint.sock")),
        Err(FailpointUdsError::ParentMetadata(_))
    ));

    let mutable = root.path().join("mutable");
    fs::create_dir(&mutable).expect("mutable ancestor");
    let mut permissions = fs::metadata(&mutable)
        .expect("mutable metadata")
        .permissions();
    permissions.set_mode(0o777);
    fs::set_permissions(&mutable, permissions).expect("mutable permissions");
    assert!(matches!(
        FailpointUdsListenerV0::bind(mutable.join("private/failpoint.sock")),
        Err(FailpointUdsError::ParentNotPrivate)
    ));
}

#[test]
fn drop_and_consume_do_not_unlink_a_same_uid_replacement() {
    let root = TestDir::new();
    let drop_path = socket_path(&root, "drop.sock");
    let listener = FailpointUdsListenerV0::bind(&drop_path).expect("bind listener");
    fs::remove_file(&drop_path).expect("replace bound path");
    fs::write(&drop_path, b"replacement-on-drop").expect("replacement");
    drop(listener);
    assert_eq!(
        fs::read(&drop_path).expect("replacement remains after drop"),
        b"replacement-on-drop"
    );

    let consume_path = socket_path(&root, "consume.sock");
    let mut listener = FailpointUdsListenerV0::bind(&consume_path).expect("bind listener");
    let mut client = connect(&consume_path);
    client
        .write_all(b"AfterEventBeforeStatus\n")
        .expect("send arm request");
    fs::remove_file(&consume_path).expect("replace bound path");
    fs::write(&consume_path, b"replacement-on-consume").expect("replacement");
    assert_eq!(
        listener.poll().expect("consume listener"),
        Some(FailpointV0::AfterEventBeforeStatus)
    );
    assert_eq!(
        fs::read(&consume_path).expect("replacement remains after consume"),
        b"replacement-on-consume"
    );
}
