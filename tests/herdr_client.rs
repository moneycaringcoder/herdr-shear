//! Wire-level tests for the herdr socket client.
//!
//! Every reply here is built from the **captured** files in `tests/capture/`,
//! which are real 0.8.0 server output. A fake that answers in the shape the
//! parser wants would pass this whole file while the parser was wrong — see
//! `tests/capture/README.md`.
//!
//! The client is not modified by these tests; they only stand a real Unix socket
//! in front of it and read the bytes it puts on the wire.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use serde_json::{json, Value};
use shear::herdr::{self, Herdr};

// ---------------------------------------------------------------------------
// A real server, one request per connection
// ---------------------------------------------------------------------------

/// A reply the fake server will make: a line of JSON, or nothing at all, which
/// is how a server that dies mid-request looks from the client's side.
type Reply = Option<String>;

struct Server {
    dir: PathBuf,
    socket: PathBuf,
    /// Every request line the server read, in order.
    requests: Arc<Mutex<Vec<String>>>,
    /// Connections that carried a request. One request per connection is the
    /// protocol, so this must always equal `requests.len()`.
    connections: Arc<Mutex<usize>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Server {
    fn new(tag: &str, replies: Vec<Reply>) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = socket_root().join(format!(
            "{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create socket directory");
        // Named `s`, not after the test: a unix socket path is capped at
        // `SUN_LEN` (108 bytes on Linux, 104 on macOS) and every byte spent on
        // a descriptive name is a byte a longer temp directory cannot have.
        // `tag` still names the directory nothing binds to, in the panic
        // messages below.
        let socket = dir.join("s");
        assert!(
            socket.as_os_str().len() < 100,
            "the socket path for `{tag}` is {} bytes, and a unix socket path is capped at \
             about 100. Point TMPDIR at something shorter — this is not a shear limit.\n  {}",
            socket.as_os_str().len(),
            socket.display()
        );

        let listener = UnixListener::bind(&socket)
            .unwrap_or_else(|err| panic!("bind the fake herdr socket for `{tag}`: {err}"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(Mutex::new(0usize));

        let stop = Arc::new(AtomicBool::new(false));
        let thread_requests = Arc::clone(&requests);
        let thread_connections = Arc::clone(&connections);
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut queue: VecDeque<Reply> = replies.into();
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_err() {
                    break;
                }
                // A connection that carries nothing is either `Herdr::connect`
                // dialling once to prove the server is there, or the shutdown
                // knock from `Drop`. Only the flag tells them apart.
                if line.trim().is_empty() {
                    if thread_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    continue;
                }
                *thread_connections.lock().unwrap() += 1;
                thread_requests
                    .lock()
                    .unwrap()
                    .push(line.trim_end().to_string());

                if let Some(Some(reply)) = queue.pop_front() {
                    let _ = (&stream).write_all(reply.as_bytes());
                    let _ = (&stream).write_all(b"\n");
                    let _ = (&stream).flush();
                }
                // The server answers one request per connection and then closes;
                // dropping the stream here is that behaviour, not a shortcut.
                drop(stream);
            }
        });

        Self {
            dir,
            socket,
            requests,
            connections,
            stop,
            handle: Some(handle),
        }
    }

    /// Connects a client pointed at this server. The environment is process
    /// wide, so the guard is held for the whole test.
    fn client(&self) -> (MutexGuard<'static, ()>, Herdr) {
        static ENV: Mutex<()> = Mutex::new(());
        let guard = ENV.lock().unwrap_or_else(|poison| poison.into_inner());
        std::env::set_var("HERDR_SOCKET_PATH", &self.socket);
        let client = Herdr::connect().expect("connect to the fake herdr");
        (guard, client)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn connections(&self) -> usize {
        *self.connections.lock().unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // An empty connection with the flag set breaks the accept loop.
        self.stop.store(true, Ordering::SeqCst);
        let _ = UnixStream::connect(&self.socket);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Where the fake sockets live.
///
/// Deliberately **not** `SHEAR_TEST_DIR`, unlike every other scratch path in the
/// suite. A unix socket path has a hard length limit — `SUN_LEN`, 108 bytes on
/// Linux and 104 on macOS — and a harness scratch directory is nested deep
/// enough to blow it on its own, which fails every test in this file with
/// `path must be shorter than SUN_LEN`. The temp directory plus a two-component
/// name is the shortest thing that is still unique per process and per server.
fn socket_root() -> PathBuf {
    std::env::temp_dir().join("shr")
}

// ---------------------------------------------------------------------------
// Captured replies
// ---------------------------------------------------------------------------

/// One of the captured files, parsed. These are whole response envelopes.
fn capture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("capture")
        .join(name);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|err| panic!("parse {}: {err}", path.display()))
}

fn line(value: &Value) -> String {
    serde_json::to_string(value).expect("encode reply")
}

fn error_reply(code: &str, message: &str) -> String {
    line(&json!({"id": "shear:1", "error": {"code": code, "message": message}}))
}

// ---------------------------------------------------------------------------
// Wire shape
// ---------------------------------------------------------------------------

/// Asserts everything that is true of *every* request shear makes, and returns
/// the decoded request so a test can go on to check its params.
fn assert_wire(raw: &str, method: &str) -> Value {
    let request: Value = serde_json::from_str(raw).unwrap_or_else(|err| {
        panic!("the client put something that is not JSON on the wire: {err}: {raw}")
    });
    let object = request
        .as_object()
        .unwrap_or_else(|| panic!("a request must be a JSON object: {raw}"));

    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["id", "method", "params"],
        "herdr's framing has no `jsonrpc` field and no other keys: {raw}"
    );

    assert!(
        request["id"].is_string(),
        "`id` must be a string, not a number: {raw}"
    );
    assert!(
        !request["id"].as_str().unwrap().is_empty(),
        "`id` must not be empty: {raw}"
    );
    assert_eq!(request["method"], json!(method));
    assert!(
        !request["params"].is_null(),
        "`params` is mandatory and must never be null: {raw}"
    );
    assert!(
        request["params"].is_object(),
        "`params` must be an object, `{{}}` when empty: {raw}"
    );
    assert!(
        !raw.contains("\n"),
        "framing is one newline-delimited request: {raw}"
    );
    request
}

// ---------------------------------------------------------------------------
// session.snapshot
// ---------------------------------------------------------------------------

#[test]
fn session_view_reads_the_arrays_from_result_snapshot() {
    let server = Server::new(
        "snapshot",
        vec![Some(line(&capture("session-snapshot.json")))],
    );
    let (_guard, mut client) = server.client();

    let view = client.session_view().expect("read the session");
    let repos = view.repos;

    let request = assert_wire(&server.requests()[0], "session.snapshot");
    assert_eq!(
        request["params"],
        json!({}),
        "an empty params object, never null"
    );

    // Two distinct repositories across eight workspaces; the four workspaces
    // with no `worktree` key are not repos and are skipped as data.
    let names: Vec<&str> = repos.iter().map(|repo| repo.name.as_str()).collect();
    assert_eq!(names, ["crescendo", "herdr-collide"]);

    let crescendo = &repos[0];
    assert_eq!(crescendo.key.0, "/home/you/repos/crescendo/.git");
    let open: Vec<(&str, &str)> = crescendo
        .open
        .iter()
        .map(|(path, workspace)| (path.to_str().unwrap(), workspace.workspace_id.as_str()))
        .collect();
    assert_eq!(
        open,
        [
            ("/home/you/repos/crescendo", "w6"),
            (
                "/home/you/.herdr/worktrees/crescendo/fix-media-fetch-throughput",
                "wE"
            ),
            (
                "/home/you/.herdr/worktrees/crescendo/fix-mart-promote-budget",
                "wY"
            ),
        ],
        "every checkout the session holds open, grouped under one repo"
    );
    assert_eq!(crescendo.open[1].1.label, "media-throughput");

    // Every pane arrives with both working directories, and the two are kept
    // apart: the shell's cwd and the foreground process's cwd are different
    // facts, and the capture's first pane proves it by having different ones.
    assert_eq!(view.panes.len(), 10, "every pane in the capture");
    let first = &view.panes[0];
    assert_eq!(first.pane_id, "wM:p1");
    assert_eq!(first.workspace_id.as_deref(), Some("wM"));
    assert_eq!(first.cwd.as_deref(), Some(Path::new("/home/you/repos")));
    assert_eq!(
        first.foreground_cwd.as_deref(),
        Some(Path::new(
            "/home/you/.local/share/mise/installs/node/24.18.0/lib/node_modules/pyright/dist"
        ))
    );
}

#[test]
fn pane_reading_treats_empty_as_absent_and_never_drops_a_placeable_pane() {
    // herdr reports absent context as an empty string, never a missing key, so
    // the empty string must arrive as `None`. A pane with neither directory can
    // occupy nothing and is dropped; a pane with a directory but no id is kept
    // under a placeholder, because an unnameable occupant is still an occupant
    // and dropping it would widen what can be removed.
    let reply = json!({
        "id": "shear:1",
        "result": {
            "type": "session_snapshot",
            "snapshot": {
                "workspaces": [],
                "panes": [
                    {"pane_id": "w1:p1", "workspace_id": "w1",
                     "cwd": "", "foreground_cwd": "/scratch/wt"},
                    {"pane_id": "w1:p2", "workspace_id": "w1",
                     "cwd": "", "foreground_cwd": ""},
                    {"workspace_id": "w2", "cwd": "/scratch/wt/deep"},
                ],
            },
        },
    });
    let server = Server::new("snapshot-pane-rules", vec![Some(line(&reply))]);
    let (_guard, mut client) = server.client();

    let panes = client.session_view().expect("read the session").panes;
    assert_eq!(
        panes.len(),
        2,
        "the pane with no directory at all is dropped"
    );

    assert_eq!(panes[0].pane_id, "w1:p1");
    assert_eq!(panes[0].cwd, None, "an empty string is absence, not a path");
    assert_eq!(
        panes[0].foreground_cwd.as_deref(),
        Some(Path::new("/scratch/wt"))
    );

    assert_eq!(panes[1].pane_id, "(pane with no id)");
    assert_eq!(panes[1].workspace_id.as_deref(), Some("w2"));
    assert_eq!(panes[1].cwd.as_deref(), Some(Path::new("/scratch/wt/deep")));
}

#[test]
fn a_reply_with_no_snapshot_key_is_a_loud_error_and_not_an_empty_session() {
    // The bug this guards: reading the arrays one level too high yields no
    // workspaces at all, which is indistinguishable from an idle session. The
    // reply below is the real capture with `snapshot` flattened away.
    let captured = capture("session-snapshot.json");
    let snapshot = captured["result"]["snapshot"].clone();
    let hoisted = json!({
        "id": "shear:1",
        "result": {
            "type": "session_snapshot",
            "workspaces": snapshot["workspaces"],
            "panes": snapshot["panes"],
        },
    });
    let server = Server::new("snapshot-missing", vec![Some(line(&hoisted))]);
    let (_guard, mut client) = server.client();

    let err = client
        .session_view()
        .expect_err("a missing `snapshot` key must be an error, not an empty session");
    let message = err.to_string();
    assert!(
        message.contains("snapshot"),
        "the error has to name the missing key: {message}"
    );
    assert!(
        message.contains("session_snapshot"),
        "and the result type it did get, so the reader can see what came back: {message}"
    );
    assert_eq!(
        herdr::error_code(&*err),
        None,
        "this is a shape failure, not a herdr error envelope"
    );
    assert_eq!(
        server.connections(),
        1,
        "a well-formed reply we cannot use is not a transport failure and is not retried"
    );
}

#[test]
fn a_checkout_path_ending_in_a_dot_still_joins_against_gits_absolute_path() {
    // herdr echoes back whatever path a workspace was created with, so one made
    // with `--cwd .` arrives as `/home/you/repos/herdr-collide/.`, which does
    // not string-match what `git worktree list` prints.
    let captured = capture("session-snapshot.json");
    let raw: Vec<&str> = captured["result"]["snapshot"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|workspace| workspace["worktree"]["checkout_path"].as_str())
        .collect();
    assert!(
        raw.contains(&"/home/you/repos/herdr-collide/."),
        "the capture must still contain the trailing-dot path this test exists for: {raw:?}"
    );

    let server = Server::new("snapshot-dot", vec![Some(line(&captured))]);
    let (_guard, mut client) = server.client();
    let repos = client.session_view().expect("read the session").repos;

    let collide = repos
        .iter()
        .find(|repo| repo.name == "herdr-collide")
        .expect("the trailing-dot workspace is still a repo");
    assert_eq!(
        collide.open[0].0,
        PathBuf::from("/home/you/repos/herdr-collide"),
        "the `.` component is dropped before any join"
    );
    assert_eq!(
        collide.root,
        PathBuf::from("/home/you/repos/herdr-collide"),
        "and from the repo root too, which is what every git call is rooted at"
    );
}

// ---------------------------------------------------------------------------
// worktree.list
// ---------------------------------------------------------------------------

#[test]
fn open_workspaces_returns_only_the_rows_that_carry_an_open_workspace_id() {
    let captured = capture("worktree-list.json");
    let rows = captured["result"]["worktrees"].as_array().unwrap().len();
    assert_eq!(rows, 6, "the capture has six worktrees, two of them open");

    let server = Server::new("worktree-list", vec![Some(line(&captured))]);
    let (_guard, mut client) = server.client();

    let open = client
        .open_workspaces(Path::new("/repos/repo"))
        .expect("list worktrees");

    let request = assert_wire(&server.requests()[0], "worktree.list");
    assert_eq!(request["params"], json!({"cwd": "/repos/repo"}));

    let open: Vec<(&str, &str)> = open
        .iter()
        .map(|(path, workspace)| (path.to_str().unwrap(), workspace.as_str()))
        .collect();
    assert_eq!(
        open,
        [("/repos/repo", "w17"), ("/repos/wt-live", "w18")],
        "the four rows with no open_workspace_id are not open in herdr"
    );
}

// ---------------------------------------------------------------------------
// worktree.remove
// ---------------------------------------------------------------------------

#[test]
fn remove_worktree_parses_the_real_worktree_removed_reply() {
    // Verbatim from docs/herdr-protocol.md, captured live on a clean worktree
    // open as workspace w18.
    let reply = line(&json!({
        "id": "shear:1",
        "result": {
            "type": "worktree_removed",
            "forced": false,
            "path": "/repos/wt-live",
            "workspace_id": "w18",
        },
    }));
    let server = Server::new("remove-ok", vec![Some(reply)]);
    let (_guard, mut client) = server.client();

    let removed = client.remove_worktree("w18", false).expect("remove");

    let request = assert_wire(&server.requests()[0], "worktree.remove");
    assert_eq!(
        request["params"],
        json!({"workspace_id": "w18", "force": false}),
        "worktree.remove is keyed by workspace, not by path"
    );
    assert_eq!(removed.path, PathBuf::from("/repos/wt-live"));
    assert_eq!(removed.workspace_id, "w18");
    assert!(!removed.forced);
    assert_eq!(server.connections(), 1);
}

#[test]
fn a_dirty_refusal_is_a_typed_error_and_is_never_retried() {
    // The exact envelope recorded in docs/herdr-protocol.md.
    let message =
        "fatal: '/repos/wt-dirty' contains modified or untracked files, use --force to delete it";
    let server = Server::new(
        "remove-dirty",
        vec![Some(error_reply(herdr::ERR_DIRTY, message))],
    );
    let (_guard, mut client) = server.client();

    let err = client
        .remove_worktree("w19", false)
        .expect_err("a dirty worktree must not be removed without force");

    assert_eq!(
        herdr::error_code(&*err),
        Some("dirty_worktree_requires_force"),
        "the guard doing its job must be distinguishable from a transport failure"
    );
    assert!(
        err.to_string().contains(message),
        "git's own text is shown verbatim: {err}"
    );
    assert_eq!(
        server.connections(),
        1,
        "a rejected request would only be rejected again, and would double-count \
         against herdr's error accounting"
    );
}

#[test]
fn a_locked_refusal_arrives_as_worktree_remove_failed_and_is_never_retried() {
    // Note the asymmetry recorded in docs/herdr-protocol.md: dirty gets its own
    // code, locked does not. Anything matching on the code alone would lump a
    // deliberately locked worktree in with a genuine failure, so the message has
    // to survive to the user unchanged.
    let message = "fatal: cannot remove a locked working tree, lock reason: held for demo";
    let server = Server::new(
        "remove-locked",
        vec![Some(error_reply(herdr::ERR_REMOVE_FAILED, message))],
    );
    let (_guard, mut client) = server.client();

    let err = client
        .remove_worktree("w20", true)
        .expect_err("a locked worktree must not be removed");

    assert_eq!(
        herdr::error_code(&*err),
        Some("worktree_remove_failed"),
        "locked shares a code with every other removal failure"
    );
    let text = err.to_string();
    assert!(
        text.contains("cannot remove a locked working tree"),
        "the lock is only visible in the message: {text}"
    );
    assert!(
        text.contains("lock reason: held for demo"),
        "including whose lock it is: {text}"
    );
    assert_eq!(server.connections(), 1, "a refusal is not retried");
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

#[test]
fn a_transport_failure_is_retried_once_with_the_same_id() {
    // The server answers one request per connection and then closes, so the
    // connection a client would reuse is already gone. The same retry is what
    // carries the client across a `herdr update --handoff`.
    let reply = line(&json!({
        "id": "shear:1",
        "result": {
            "type": "worktree_removed",
            "forced": true,
            "path": "/repos/wt-live",
            "workspace_id": "w18",
        },
    }));
    let server = Server::new("retry", vec![None, Some(reply)]);
    let (_guard, mut client) = server.client();

    let removed = client
        .remove_worktree("w18", true)
        .expect("the retry carries the call");
    assert!(removed.forced);

    let requests = server.requests();
    assert_eq!(requests.len(), 2, "one attempt, then one retry");
    assert_eq!(server.connections(), 2, "each on its own connection");
    let first = assert_wire(&requests[0], "worktree.remove");
    let second = assert_wire(&requests[1], "worktree.remove");
    assert_eq!(
        first["id"], second["id"],
        "a retry is the same request, not a new one"
    );
    assert_eq!(first["params"], second["params"]);
}

#[test]
fn every_call_gets_its_own_connection_and_its_own_id() {
    let server = Server::new(
        "one-per-connection",
        vec![
            Some(line(&capture("worktree-list.json"))),
            Some(line(&json!({
                "id": "shear:2",
                "result": {
                    "type": "worktree_removed",
                    "forced": false,
                    "path": "/repos/wt-live",
                    "workspace_id": "w18",
                },
            }))),
        ],
    );
    let (_guard, mut client) = server.client();

    client
        .open_workspaces(Path::new("/repos/repo"))
        .expect("list");
    client.remove_worktree("w18", false).expect("remove");

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        server.connections(),
        requests.len(),
        "one request per connection, never two down one socket"
    );
    let first = assert_wire(&requests[0], "worktree.list");
    let second = assert_wire(&requests[1], "worktree.remove");
    assert_ne!(
        first["id"], second["id"],
        "distinct calls carry distinct ids"
    );
}
