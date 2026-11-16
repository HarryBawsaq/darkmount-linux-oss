//! Discord's local IPC protocol, implemented directly on a Unix socket.
//!
//! Every frame is a little-endian `u32` opcode, a little-endian `u32`
//! payload length, then that many bytes of JSON.

use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

pub const OP_HANDSHAKE: u32 = 0;
pub const OP_FRAME: u32 = 1;
pub const OP_CLOSE: u32 = 2;
pub const OP_PING: u32 = 3;
pub const OP_PONG: u32 = 4;

const HEADER_LEN: usize = 8;
/// Discord's replies are a few kilobytes; anything larger is not Discord.
const MAX_PAYLOAD: usize = 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(3);

/// Socket directories relative to the runtime directory: native Discord,
/// Flatpak Discord, Flatpak Vesktop, Snap Discord.
const SOCKET_SUBDIRS: &[&str] = &[
    "",
    "app/com.discordapp.Discord",
    ".flatpak/dev.vencord.Vesktop/xdg-run",
    "snap.discord",
];

pub fn encode_frame(opcode: u32, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&opcode.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Decodes the frame at the start of `buf`.
///
/// Returns the opcode, the payload and the number of bytes consumed, or
/// `None` when the frame is not complete yet.
pub fn decode_frame(buf: &[u8]) -> io::Result<Option<(u32, Vec<u8>, usize)>> {
    if buf.len() < HEADER_LEN {
        return Ok(None);
    }
    let opcode = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds the {MAX_PAYLOAD} byte limit"),
        ));
    }
    let end = HEADER_LEN + len;
    if buf.len() < end {
        return Ok(None);
    }
    Ok(Some((opcode, buf[HEADER_LEN..end].to_vec(), end)))
}

/// Blocks until one whole frame is available. `buf` holds bytes read past
/// the end of the previous frame.
pub fn read_frame(reader: &mut impl Read, buf: &mut Vec<u8>) -> io::Result<(u32, Vec<u8>)> {
    let mut chunk = [0u8; 4096];
    loop {
        if let Some((opcode, payload, used)) = decode_frame(buf)? {
            buf.drain(..used);
            return Ok((opcode, payload));
        }
        match reader.read(&mut chunk) {
            Ok(0) => return Err(closed()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

fn closed() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "Discord closed the connection",
    )
}

/// The directory Discord creates its socket in for the current session.
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Ok(meta) = fs::metadata("/proc/self") {
        let dir = PathBuf::from(format!("/run/user/{}", meta.uid()));
        if dir.is_dir() {
            return dir;
        }
    }
    env::temp_dir()
}

/// The directories that may hold a Discord socket, given the runtime dir.
pub fn socket_dirs(runtime_dir: &Path) -> Vec<PathBuf> {
    SOCKET_SUBDIRS
        .iter()
        .map(|sub| runtime_dir.join(sub))
        .collect()
}

/// `discord-ipc-0` through `discord-ipc-9` in each directory, in order.
pub fn socket_candidates(dirs: &[PathBuf]) -> Vec<PathBuf> {
    dirs.iter()
        .flat_map(|dir| (0..10).map(move |n| dir.join(format!("discord-ipc-{n}"))))
        .collect()
}

/// The first socket that accepts a connection, without a handshake.
pub fn probe(dirs: &[PathBuf]) -> Option<PathBuf> {
    socket_candidates(dirs)
        .into_iter()
        .find(|path| UnixStream::connect(path).is_ok())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Activity {
    pub details: String,
    pub state: String,
    pub timestamps: Timestamps,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assets: Option<Assets>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Timestamps {
    /// Unix time in milliseconds.
    pub start: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Assets {
    pub large_image: String,
    pub large_text: String,
}

/// The `SET_ACTIVITY` command. No activity clears the presence.
pub fn set_activity_payload(pid: u32, activity: Option<&Activity>, nonce: &str) -> Value {
    let mut args = json!({ "pid": pid });
    if let Some(activity) = activity {
        args["activity"] = json!(activity);
    }
    json!({ "cmd": "SET_ACTIVITY", "args": args, "nonce": nonce })
}

#[derive(Debug)]
pub enum ConnectError {
    /// No socket accepted a connection: Discord is closed.
    NotRunning,
    /// Discord answered the handshake with an error, e.g. a bad client ID.
    Rejected(String),
    Io(io::Error),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::NotRunning => write!(f, "Discord is not running"),
            ConnectError::Rejected(why) => write!(f, "Discord rejected the handshake: {why}"),
            ConnectError::Io(e) => write!(f, "handshake failed: {e}"),
        }
    }
}

/// An open, handshaken connection for one client ID.
#[derive(Debug)]
pub struct Connection {
    stream: UnixStream,
    path: PathBuf,
    client_id: String,
    buf: Vec<u8>,
    nonce: u64,
}

impl Connection {
    /// Connects to the first Discord socket that completes the handshake.
    pub fn connect(dirs: &[PathBuf], client_id: &str) -> Result<Connection, ConnectError> {
        let mut failure = ConnectError::NotRunning;
        for path in socket_candidates(dirs) {
            // A missing or stale socket file just means "not this one".
            let Ok(stream) = UnixStream::connect(&path) else {
                continue;
            };
            match Connection::handshake(stream, path, client_id) {
                Ok(connection) => return Ok(connection),
                Err(e) => failure = e,
            }
        }
        Err(failure)
    }

    fn handshake(
        stream: UnixStream,
        path: PathBuf,
        client_id: &str,
    ) -> Result<Connection, ConnectError> {
        let mut connection = Connection {
            stream,
            path,
            client_id: client_id.to_string(),
            buf: Vec::new(),
            nonce: 0,
        };
        let reply = (|| {
            connection.stream.set_read_timeout(Some(IO_TIMEOUT))?;
            connection.stream.set_write_timeout(Some(IO_TIMEOUT))?;
            connection.write_frame(OP_HANDSHAKE, &json!({ "v": 1, "client_id": client_id }))?;
            read_frame(&mut connection.stream, &mut connection.buf)
        })()
        .map_err(ConnectError::Io)?;

        let (opcode, payload) = reply;
        let body: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
        match opcode {
            OP_FRAME if body["evt"] == "READY" => Ok(connection),
            OP_FRAME | OP_CLOSE => Err(ConnectError::Rejected(describe(&body))),
            other => Err(ConnectError::Rejected(format!("unexpected opcode {other}"))),
        }
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Sets the activity, or clears it with `None`.
    pub fn set_activity(&mut self, activity: Option<&Activity>) -> io::Result<()> {
        self.nonce += 1;
        let nonce = format!("{}-{}", std::process::id(), self.nonce);
        let payload = set_activity_payload(std::process::id(), activity, &nonce);
        self.write_frame(OP_FRAME, &payload)
    }

    /// Reads whatever Discord has sent without blocking. Returns an error
    /// when the connection is gone: EOF, a CLOSE frame, or an I/O failure.
    pub fn pump(&mut self) -> io::Result<()> {
        self.stream.set_nonblocking(true)?;
        let read = self.read_available();
        self.stream.set_nonblocking(false)?;
        read?;

        while let Some((opcode, payload, used)) = decode_frame(&self.buf)? {
            self.buf.drain(..used);
            let body: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
            match opcode {
                OP_CLOSE => {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        format!("Discord closed the connection: {}", describe(&body)),
                    ))
                }
                OP_PING => self.write_frame(OP_PONG, &body)?,
                OP_FRAME if body["evt"] == "ERROR" => {
                    log::warn!("Discord rejected a command: {}", describe(&body["data"]));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn read_available(&mut self) -> io::Result<()> {
        let mut chunk = [0u8; 4096];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(closed()),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn write_frame(&mut self, opcode: u32, payload: &Value) -> io::Result<()> {
        let frame = encode_frame(opcode, payload.to_string().as_bytes());
        self.stream.write_all(&frame)
    }
}

/// Renders Discord's `{"code": ..., "message": ...}` error objects.
fn describe(body: &Value) -> String {
    match (body["message"].as_str(), body["code"].as_i64()) {
        (Some(message), Some(code)) => format!("{message} (code {code})"),
        (Some(message), None) => message.to_string(),
        _ => body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeDiscord, TempDir};

    #[test]
    fn encodes_header_as_little_endian_opcode_and_length() {
        let frame = encode_frame(OP_FRAME, b"{\"a\":1}");
        assert_eq!(&frame[..4], &[1, 0, 0, 0]);
        assert_eq!(&frame[4..8], &[7, 0, 0, 0]);
        assert_eq!(&frame[8..], b"{\"a\":1}");

        let big = encode_frame(0x0102_0304, &[0u8; 0x0105]);
        assert_eq!(&big[..8], &[0x04, 0x03, 0x02, 0x01, 0x05, 0x01, 0, 0]);
        assert_eq!(big.len(), 8 + 0x0105);

        assert_eq!(encode_frame(OP_HANDSHAKE, b""), [0u8; 8]);
    }

    #[test]
    fn handshake_frame_matches_the_protocol() {
        let payload = json!({ "v": 1, "client_id": "123" }).to_string();
        assert_eq!(payload, r#"{"client_id":"123","v":1}"#);
        let frame = encode_frame(OP_HANDSHAKE, payload.as_bytes());
        assert_eq!(&frame[..8], &[0, 0, 0, 0, 25, 0, 0, 0]);
    }

    #[test]
    fn decodes_complete_frames_and_waits_for_partial_ones() {
        let mut bytes = encode_frame(OP_PING, b"abc");
        bytes.extend(encode_frame(OP_FRAME, b"{}"));

        for cut in 0..11 {
            assert_eq!(decode_frame(&bytes[..cut]).unwrap(), None, "cut at {cut}");
        }
        let (opcode, payload, used) = decode_frame(&bytes).unwrap().unwrap();
        assert_eq!(
            (opcode, payload.as_slice(), used),
            (OP_PING, &b"abc"[..], 11)
        );
        let (opcode, payload, used) = decode_frame(&bytes[used..]).unwrap().unwrap();
        assert_eq!(
            (opcode, payload.as_slice(), used),
            (OP_FRAME, &b"{}"[..], 10)
        );
    }

    #[test]
    fn rejects_oversized_frames() {
        let mut header = OP_FRAME.to_le_bytes().to_vec();
        header.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_frame(&header).is_err());
    }

    #[test]
    fn read_frame_reassembles_split_input() {
        struct OneByte<'a>(&'a [u8]);
        impl Read for OneByte<'_> {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                let Some((first, rest)) = self.0.split_first() else {
                    return Ok(0);
                };
                out[0] = *first;
                self.0 = rest;
                Ok(1)
            }
        }
        let mut bytes = encode_frame(OP_FRAME, b"{\"x\":1}");
        bytes.extend(encode_frame(OP_CLOSE, b"{}"));
        let mut reader = OneByte(&bytes);
        let mut buf = Vec::new();

        assert_eq!(
            read_frame(&mut reader, &mut buf).unwrap(),
            (OP_FRAME, b"{\"x\":1}".to_vec())
        );
        assert_eq!(
            read_frame(&mut reader, &mut buf).unwrap(),
            (OP_CLOSE, b"{}".to_vec())
        );
        let eof = read_frame(&mut reader, &mut buf).unwrap_err();
        assert_eq!(eof.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn set_activity_payload_has_the_documented_shape() {
        let activity = Activity {
            details: "ELDEN RING".into(),
            state: "Playing via Proton".into(),
            timestamps: Timestamps {
                start: 1_700_000_000_000,
            },
            assets: None,
        };
        assert_eq!(
            set_activity_payload(42, Some(&activity), "n1"),
            json!({
                "cmd": "SET_ACTIVITY",
                "args": {
                    "pid": 42,
                    "activity": {
                        "details": "ELDEN RING",
                        "state": "Playing via Proton",
                        "timestamps": { "start": 1_700_000_000_000u64 },
                    },
                },
                "nonce": "n1",
            })
        );

        let with_art = Activity {
            assets: Some(Assets {
                large_image: "elden_ring".into(),
                large_text: "ELDEN RING".into(),
            }),
            ..activity
        };
        let payload = set_activity_payload(42, Some(&with_art), "n2");
        assert_eq!(
            payload["args"]["activity"]["assets"],
            json!({ "large_image": "elden_ring", "large_text": "ELDEN RING" })
        );

        let clear = set_activity_payload(42, None, "n3");
        assert_eq!(clear["args"], json!({ "pid": 42 }));
    }

    #[test]
    fn lists_sockets_for_native_flatpak_and_snap() {
        let dirs = socket_dirs(Path::new("/run/user/1000"));
        let candidates = socket_candidates(&dirs);
        assert_eq!(candidates.len(), 40);
        assert_eq!(candidates[0], Path::new("/run/user/1000/discord-ipc-0"));
        assert_eq!(candidates[9], Path::new("/run/user/1000/discord-ipc-9"));
        for expected in [
            "/run/user/1000/app/com.discordapp.Discord/discord-ipc-0",
            "/run/user/1000/.flatpak/dev.vencord.Vesktop/xdg-run/discord-ipc-0",
            "/run/user/1000/snap.discord/discord-ipc-3",
        ] {
            assert!(candidates.contains(&PathBuf::from(expected)), "{expected}");
        }
    }

    #[test]
    fn connect_reports_not_running_without_a_socket() {
        let tmp = TempDir::new("discord");
        let dirs = socket_dirs(tmp.path());
        assert!(matches!(
            Connection::connect(&dirs, "1"),
            Err(ConnectError::NotRunning)
        ));
        assert_eq!(probe(&dirs), None);

        // A stale socket file left behind by a crashed Discord.
        drop(std::os::unix::net::UnixListener::bind(tmp.path().join("discord-ipc-0")).unwrap());
        assert!(matches!(
            Connection::connect(&dirs, "1"),
            Err(ConnectError::NotRunning)
        ));
        assert_eq!(probe(&dirs), None);
    }

    #[test]
    fn handshakes_and_sets_activity() {
        let tmp = TempDir::new("discord");
        let dirs = socket_dirs(tmp.path());
        let discord = FakeDiscord::start(&tmp.path().join("discord-ipc-0"));

        assert_eq!(probe(&dirs), Some(tmp.path().join("discord-ipc-0")));
        let mut connection = Connection::connect(&dirs, "123").unwrap();
        assert_eq!(connection.client_id(), "123");
        assert_eq!(
            discord.recv(),
            (OP_HANDSHAKE, json!({ "v": 1, "client_id": "123" }))
        );

        let activity = Activity {
            details: "Game".into(),
            state: "Playing via Proton".into(),
            timestamps: Timestamps { start: 5 },
            assets: None,
        };
        connection.set_activity(Some(&activity)).unwrap();
        let (opcode, body) = discord.recv();
        assert_eq!(opcode, OP_FRAME);
        assert_eq!(body["cmd"], "SET_ACTIVITY");
        assert_eq!(body["args"]["pid"], std::process::id());
        assert_eq!(body["args"]["activity"]["details"], "Game");

        connection.set_activity(None).unwrap();
        let (_, body) = discord.recv();
        assert_eq!(body["args"].get("activity"), None);
        assert_ne!(body["nonce"], Value::Null);

        connection.pump().unwrap();
    }

    #[test]
    fn finds_discord_in_the_flatpak_directory_and_on_a_later_socket() {
        let tmp = TempDir::new("discord");
        let dirs = socket_dirs(tmp.path());
        let flatpak = tmp.path().join("app/com.discordapp.Discord");
        fs::create_dir_all(&flatpak).unwrap();
        let _discord = FakeDiscord::start(&flatpak.join("discord-ipc-1"));

        let connection = Connection::connect(&dirs, "1").unwrap();
        assert_eq!(connection.path(), flatpak.join("discord-ipc-1"));
    }

    #[test]
    fn rejected_handshake_is_reported() {
        let tmp = TempDir::new("discord");
        let _discord = FakeDiscord::start_rejecting(&tmp.path().join("discord-ipc-0"));
        match Connection::connect(&socket_dirs(tmp.path()), "bad") {
            Err(ConnectError::Rejected(why)) => {
                assert_eq!(why, "Invalid Client ID (code 4000)")
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn pump_answers_ping_and_detects_disconnect() {
        let tmp = TempDir::new("discord");
        let discord = FakeDiscord::start(&tmp.path().join("discord-ipc-0"));
        let mut connection = Connection::connect(&socket_dirs(tmp.path()), "1").unwrap();
        discord.recv();

        discord.send(OP_PING, &json!({ "seq": 7 }));
        // The ping is in flight; poll until it has been answered.
        let pong = (0..200).find_map(|_| {
            connection.pump().unwrap();
            discord.try_recv(Duration::from_millis(10))
        });
        assert_eq!(pong, Some((OP_PONG, json!({ "seq": 7 }))));

        discord.stop();
        let err = connection.pump().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(connection.set_activity(None).is_err());
    }
}
