//! The polling state machine: detect the game, keep Discord in sync.
//!
//! [`Daemon::tick`] is one poll. It takes the current time as arguments and
//! never sleeps, so the whole lifecycle can be driven from tests.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use log::{debug, error, info, warn};

use crate::config::Config;
use crate::detect::{self, RunningGame};
use crate::discord::{self, Activity, Assets, ConnectError, Connection, Timestamps};
use crate::steam;

pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
const BACKOFF_INITIAL: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Discord's limit for the `details` field.
const MAX_DETAILS_CHARS: usize = 128;

/// Reconnect delay that doubles from `initial` and then stays at `max`.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    next: Duration,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration) -> Backoff {
        Backoff {
            initial,
            max,
            next: initial,
        }
    }

    /// The delay to wait before the next attempt.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (self.next * 2).min(self.max);
        delay
    }

    pub fn reset(&mut self) {
        self.next = self.initial;
    }
}

/// Every location the daemon touches, so tests can point it elsewhere.
#[derive(Debug, Clone)]
pub struct Paths {
    pub proc_root: PathBuf,
    pub home: PathBuf,
    pub socket_dirs: Vec<PathBuf>,
    pub config_path: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Paths> {
        let home = env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")?;
        Ok(Paths {
            proc_root: PathBuf::from("/proc"),
            socket_dirs: discord::socket_dirs(&discord::runtime_dir()),
            config_path: home.join(".config/proton-presence/config.toml"),
            home,
        })
    }
}

/// The game being shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    pub app_id: u64,
    pub pid: u32,
    pub name: String,
    pub proton: bool,
    /// Unix time in milliseconds when the game was first detected.
    pub started_ms: u64,
}

impl Game {
    pub fn from_running(running: &RunningGame, home: &Path, now_ms: u64) -> Game {
        Game {
            app_id: running.app_id,
            pid: running.pid,
            name: steam::app_name(home, running.app_id)
                .unwrap_or_else(|| format!("Steam app {}", running.app_id)),
            proton: running.proton,
            started_ms: now_ms,
        }
    }

    pub fn state_line(&self) -> &'static str {
        if self.proton {
            "Playing via Proton"
        } else {
            "Playing on Steam"
        }
    }
}

/// Scans for the game a fresh daemon would show right now.
pub fn detect_game(paths: &Paths, now_ms: u64) -> Option<Game> {
    let games = detect::scan(&paths.proc_root);
    detect::pick_active(&games, None).map(|g| Game::from_running(g, &paths.home, now_ms))
}

pub fn build_activity(game: &Game, large_image: Option<&str>) -> Activity {
    let details: String = game.name.chars().take(MAX_DETAILS_CHARS).collect();
    Activity {
        assets: large_image.map(|key| Assets {
            large_image: key.to_string(),
            large_text: details.clone(),
        }),
        details,
        state: game.state_line().to_string(),
        timestamps: Timestamps {
            start: game.started_ms,
        },
    }
}

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub struct Daemon {
    paths: Paths,
    config: Config,
    /// Modification time and size of the config file when last read.
    config_stamp: Option<(SystemTime, u64)>,
    game: Option<Game>,
    connection: Option<Connection>,
    /// The activity Discord currently shows for this connection.
    published: Option<Activity>,
    backoff: Backoff,
    retry_at: Option<Instant>,
    /// Last connect failure that was logged, to log each cause once.
    last_failure: Option<String>,
    /// App ID already warned about for having no client ID.
    warned_unmapped: Option<u64>,
    connect_attempts: u64,
}

impl Daemon {
    pub fn new(paths: Paths) -> Daemon {
        let mut daemon = Daemon {
            paths,
            config: Config::default(),
            config_stamp: None,
            game: None,
            connection: None,
            published: None,
            backoff: Backoff::new(BACKOFF_INITIAL, BACKOFF_MAX),
            retry_at: None,
            last_failure: None,
            warned_unmapped: None,
            connect_attempts: 0,
        };
        daemon.config_stamp = daemon.config_stamp();
        daemon.load_config();
        daemon
    }

    /// One poll: `now` drives the reconnect schedule, `now_ms` (Unix time
    /// in milliseconds) stamps a newly detected game.
    pub fn tick(&mut self, now: Instant, now_ms: u64) {
        let stamp = self.config_stamp();
        if stamp != self.config_stamp {
            self.config_stamp = stamp;
            info!("Config changed, reloading");
            self.load_config();
        }
        self.update_game(now_ms);
        self.sync_presence(now);
    }

    fn config_stamp(&self) -> Option<(SystemTime, u64)> {
        let meta = fs::metadata(&self.paths.config_path).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    }

    fn load_config(&mut self) {
        match Config::load(&self.paths.config_path) {
            Ok(config) => {
                if config.default_client_id.is_none() {
                    warn!(
                        "No default_client_id in {}: games without their own mapping get no presence",
                        self.paths.config_path.display()
                    );
                }
                self.config = config;
            }
            // Keep running on the previous settings; the file is re-read
            // as soon as it changes again.
            Err(e) => error!("Ignoring invalid config: {e:#}"),
        }
        // A fixed config should take effect on the next poll, not after
        // the current backoff delay.
        self.warned_unmapped = None;
        self.last_failure = None;
        self.retry_at = None;
        self.backoff.reset();
    }

    fn update_game(&mut self, now_ms: u64) {
        let games = detect::scan(&self.paths.proc_root);
        let current = self.game.as_ref().map(|g| g.app_id);
        let active = detect::pick_active(&games, current);

        if let (Some(game), Some(active)) = (&mut self.game, active) {
            if game.app_id == active.app_id {
                // Wine processes appear a moment after the launcher does.
                game.proton |= active.proton;
                game.pid = active.pid;
                return;
            }
        }
        if let Some(ended) = self.game.take() {
            info!("Game ended: {} (app {})", ended.name, ended.app_id);
        }
        if let Some(active) = active {
            let game = Game::from_running(active, &self.paths.home, now_ms);
            info!("Game started: {} (app {})", game.name, game.app_id);
            self.game = Some(game);
        }
    }

    fn sync_presence(&mut self, now: Instant) {
        let Some(game) = &self.game else {
            self.close_connection();
            return;
        };
        let Some(resolved) = self.config.resolve(game.app_id) else {
            if self.warned_unmapped != Some(game.app_id) {
                warn!(
                    "No Discord client ID for {} (app {}): set default_client_id or [games.{}] in {}",
                    game.name,
                    game.app_id,
                    game.app_id,
                    self.paths.config_path.display()
                );
                self.warned_unmapped = Some(game.app_id);
            }
            self.close_connection();
            return;
        };
        let client_id = resolved.client_id.to_string();
        let desired = build_activity(game, resolved.large_image);

        // The client ID is fixed at handshake time, so another game's
        // application needs a new connection.
        if self
            .connection
            .as_ref()
            .is_some_and(|c| c.client_id() != client_id)
        {
            self.close_connection();
        }

        if let Some(connection) = &mut self.connection {
            if let Err(e) = connection.pump() {
                self.connection_lost(&e.to_string());
            }
        }

        if self.connection.is_none() {
            if self.retry_at.is_some_and(|due| now < due) {
                return;
            }
            self.connect_attempts += 1;
            match Connection::connect(&self.paths.socket_dirs, &client_id) {
                Ok(connection) => {
                    info!(
                        "Connected to Discord at {} (client ID {client_id})",
                        connection.path().display()
                    );
                    self.connection = Some(connection);
                    self.backoff.reset();
                    self.retry_at = None;
                    self.last_failure = None;
                }
                Err(e) => {
                    let delay = self.backoff.next_delay();
                    self.retry_at = Some(now + delay);
                    let reason = e.to_string();
                    if self.last_failure.as_deref() != Some(reason.as_str()) {
                        match e {
                            ConnectError::NotRunning => {
                                info!("{reason}; presence resumes when it starts")
                            }
                            _ => warn!("Cannot connect to Discord: {reason}; retrying"),
                        }
                        self.last_failure = Some(reason);
                    }
                    debug!("Next connection attempt in {}s", delay.as_secs());
                    return;
                }
            }
        }

        if self.published.as_ref() != Some(&desired) {
            let Some(connection) = &mut self.connection else {
                return;
            };
            match connection.set_activity(Some(&desired)) {
                Ok(()) => {
                    debug!("Presence set: {} / {}", desired.details, desired.state);
                    self.published = Some(desired);
                }
                Err(e) => self.connection_lost(&e.to_string()),
            }
        }
    }

    /// Deliberate close: clear the presence first, then hang up.
    fn close_connection(&mut self) {
        if let Some(mut connection) = self.connection.take() {
            let _ = connection.set_activity(None);
            info!("Presence cleared, disconnected from Discord");
        }
        self.published = None;
        self.retry_at = None;
        self.last_failure = None;
        self.backoff.reset();
    }

    /// Discord went away. Forget what it showed and reconnect from scratch.
    fn connection_lost(&mut self, reason: &str) {
        info!("Disconnected from Discord ({reason}); reconnecting when it is back");
        self.connection = None;
        self.published = None;
        self.retry_at = None;
        self.backoff.reset();
    }

    #[cfg(test)]
    pub fn is_connected(&self) -> bool {
        self.connection.is_some()
    }

    #[cfg(test)]
    pub fn connect_attempts(&self) -> u64 {
        self.connect_attempts
    }

    #[cfg(test)]
    pub fn game(&self) -> Option<&Game> {
        self.game.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::{OP_FRAME, OP_HANDSHAKE};
    use crate::testutil::{add_proc, FakeDiscord, TempDir};
    use serde_json::{json, Value};

    const ELDEN_RING: u64 = 1245620;
    const PORTAL: u64 = 400;

    struct Fixture {
        tmp: TempDir,
        paths: Paths,
        start: Instant,
    }

    impl Fixture {
        fn new(config: Option<&str>) -> Fixture {
            let tmp = TempDir::new("daemon");
            let paths = Paths {
                proc_root: tmp.path().join("proc"),
                home: tmp.path().join("home"),
                socket_dirs: discord::socket_dirs(&tmp.path().join("run")),
                config_path: tmp.path().join("config.toml"),
            };
            let steamapps = paths.home.join(".local/share/Steam/steamapps");
            fs::create_dir_all(&steamapps).unwrap();
            fs::create_dir_all(&paths.proc_root).unwrap();
            fs::create_dir_all(tmp.path().join("run")).unwrap();
            fs::write(
                steamapps.join(format!("appmanifest_{ELDEN_RING}.acf")),
                r#""AppState" { "appid" "1245620" "name" "ELDEN RING" }"#,
            )
            .unwrap();
            fs::write(
                steamapps.join(format!("appmanifest_{PORTAL}.acf")),
                r#""AppState" { "appid" "400" "name" "Portal" }"#,
            )
            .unwrap();
            if let Some(config) = config {
                fs::write(&paths.config_path, config).unwrap();
            }
            Fixture {
                tmp,
                paths,
                start: Instant::now(),
            }
        }

        fn daemon(&self) -> Daemon {
            Daemon::new(self.paths.clone())
        }

        fn socket(&self) -> PathBuf {
            self.tmp.path().join("run/discord-ipc-0")
        }

        /// The instant `secs` seconds into the test's timeline.
        fn at(&self, secs: u64) -> Instant {
            self.start + Duration::from_secs(secs)
        }

        fn start_proton_game(&self, pid: u32, app_id: u64) {
            let id = app_id.to_string();
            let env = [("SteamAppId", id.as_str()), ("WINEPREFIX", "/x/pfx")];
            add_proc(
                &self.paths.proc_root,
                pid - 1,
                "wineserver",
                &["wineserver"],
                &env,
            );
            add_proc(
                &self.paths.proc_root,
                pid,
                "game.exe",
                &["Z:\\g\\game.exe"],
                &env,
            );
        }

        fn stop_game(&self, pid: u32) {
            // The game exits first; wineserver lingers.
            fs::remove_dir_all(self.paths.proc_root.join(pid.to_string())).unwrap();
        }
    }

    /// Asserts the next two frames are a handshake and a SET_ACTIVITY, and
    /// returns the activity.
    fn expect_presence(discord: &FakeDiscord, client_id: &str) -> Value {
        assert_eq!(
            discord.recv(),
            (OP_HANDSHAKE, json!({ "v": 1, "client_id": client_id }))
        );
        let (opcode, body) = discord.recv();
        assert_eq!(opcode, OP_FRAME);
        assert_eq!(body["cmd"], "SET_ACTIVITY");
        body["args"]["activity"].clone()
    }

    #[test]
    fn backoff_grows_from_5s_to_60s_and_stays_there() {
        let mut backoff = Backoff::new(BACKOFF_INITIAL, BACKOFF_MAX);
        let delays: Vec<u64> = (0..8).map(|_| backoff.next_delay().as_secs()).collect();
        assert_eq!(delays, [5, 10, 20, 40, 60, 60, 60, 60]);

        backoff.reset();
        assert_eq!(backoff.next_delay().as_secs(), 5);
        assert_eq!(backoff.next_delay().as_secs(), 10);
    }

    #[test]
    fn builds_activity_from_game_and_config() {
        let mut game = Game {
            app_id: ELDEN_RING,
            pid: 1,
            name: "ELDEN RING".into(),
            proton: true,
            started_ms: 1_700_000_000_000,
        };
        let activity = build_activity(&game, None);
        assert_eq!(activity.details, "ELDEN RING");
        assert_eq!(activity.state, "Playing via Proton");
        assert_eq!(activity.timestamps.start, 1_700_000_000_000);
        assert_eq!(activity.assets, None);

        let assets = build_activity(&game, Some("elden_ring")).assets.unwrap();
        assert_eq!(assets.large_image, "elden_ring");
        assert_eq!(assets.large_text, "ELDEN RING");

        game.proton = false;
        assert_eq!(build_activity(&game, None).state, "Playing on Steam");

        game.name = "é".repeat(200);
        assert_eq!(build_activity(&game, None).details.chars().count(), 128);
    }

    #[test]
    fn idle_daemon_never_touches_discord() {
        let fx = Fixture::new(Some("default_client_id = \"100\"\n"));
        let discord = FakeDiscord::start(&fx.socket());
        let mut daemon = fx.daemon();
        for secs in (0..60).step_by(5) {
            daemon.tick(fx.at(secs), 0);
        }
        assert_eq!(daemon.connect_attempts(), 0);
        assert_eq!(daemon.game(), None);
        assert_eq!(discord.try_recv(Duration::from_millis(50)), None);
    }

    #[test]
    fn retries_with_backoff_while_discord_is_closed() {
        let fx = Fixture::new(Some("default_client_id = \"100\"\n"));
        fx.start_proton_game(1000, ELDEN_RING);
        let mut daemon = fx.daemon();

        let mut attempt_times = Vec::new();
        for secs in (0..=400).step_by(5) {
            let before = daemon.connect_attempts();
            daemon.tick(fx.at(secs), 0);
            if daemon.connect_attempts() > before {
                attempt_times.push(secs);
            }
            assert!(!daemon.is_connected());
        }
        // Gaps of 5, 10, 20, 40 and then 60 seconds.
        assert_eq!(attempt_times, [0, 5, 15, 35, 75, 135, 195, 255, 315, 375]);
        assert_eq!(daemon.game().unwrap().name, "ELDEN RING");
    }

    #[test]
    fn connects_when_discord_appears_and_reconnects_after_restart() {
        let fx = Fixture::new(Some(
            "default_client_id = \"100\"\n[games.1245620]\nlarge_image = \"elden_ring\"\n",
        ));
        fx.start_proton_game(1000, ELDEN_RING);
        let mut daemon = fx.daemon();

        // Discord is closed at startup: the daemon waits.
        daemon.tick(fx.at(0), 111_000);
        assert!(!daemon.is_connected());
        assert_eq!(daemon.connect_attempts(), 1);

        // Discord starts; the retry scheduled for +5s connects.
        let discord = FakeDiscord::start(&fx.socket());
        daemon.tick(fx.at(5), 116_000);
        assert!(daemon.is_connected());
        let activity = expect_presence(&discord, "100");
        assert_eq!(
            activity,
            json!({
                "details": "ELDEN RING",
                "state": "Playing via Proton",
                "timestamps": { "start": 111_000 },
                "assets": { "large_image": "elden_ring", "large_text": "ELDEN RING" },
            })
        );

        // Nothing changed, so nothing is sent.
        daemon.tick(fx.at(10), 121_000);
        assert_eq!(discord.try_recv(Duration::from_millis(50)), None);

        // Discord quits: the daemon notices and starts retrying.
        discord.stop();
        daemon.tick(fx.at(15), 126_000);
        assert!(!daemon.is_connected());
        assert_eq!(daemon.connect_attempts(), 3);
        daemon.tick(fx.at(20), 131_000);
        assert_eq!(daemon.connect_attempts(), 4);

        // Discord is back, but the next attempt is not due until +30s.
        let discord = FakeDiscord::start(&fx.socket());
        daemon.tick(fx.at(25), 136_000);
        assert!(!daemon.is_connected());
        assert_eq!(daemon.connect_attempts(), 4);

        daemon.tick(fx.at(30), 141_000);
        assert!(daemon.is_connected());
        let activity = expect_presence(&discord, "100");
        assert_eq!(activity["details"], "ELDEN RING");
        assert_eq!(
            activity["timestamps"]["start"], 111_000,
            "the start time must survive a reconnect"
        );

        // A successful connection resets the backoff to 5s.
        discord.stop();
        daemon.tick(fx.at(35), 146_000);
        assert_eq!(daemon.connect_attempts(), 6);
        daemon.tick(fx.at(40), 151_000);
        assert_eq!(daemon.connect_attempts(), 7);
    }

    #[test]
    fn clears_presence_when_the_game_ends() {
        let fx = Fixture::new(Some("default_client_id = \"100\"\n"));
        let discord = FakeDiscord::start(&fx.socket());
        fx.start_proton_game(1000, ELDEN_RING);
        let mut daemon = fx.daemon();

        daemon.tick(fx.at(0), 1_000);
        expect_presence(&discord, "100");

        fx.stop_game(1000);
        daemon.tick(fx.at(5), 6_000);
        assert_eq!(daemon.game(), None);
        assert!(!daemon.is_connected());
        let (opcode, body) = discord.recv();
        assert_eq!(opcode, OP_FRAME);
        assert_eq!(body["cmd"], "SET_ACTIVITY");
        assert_eq!(
            body["args"].get("activity"),
            None,
            "activity must be cleared"
        );

        // Starting it again is a new session with a new start time.
        fx.start_proton_game(1000, ELDEN_RING);
        daemon.tick(fx.at(10), 11_000);
        let activity = expect_presence(&discord, "100");
        assert_eq!(activity["timestamps"]["start"], 11_000);
    }

    #[test]
    fn switches_client_id_per_game() {
        let fx = Fixture::new(Some(
            "default_client_id = \"100\"\n[games.400]\nclient_id = \"200\"\n",
        ));
        let discord = FakeDiscord::start(&fx.socket());
        let mut daemon = fx.daemon();

        fx.start_proton_game(1000, ELDEN_RING);
        daemon.tick(fx.at(0), 1_000);
        assert_eq!(expect_presence(&discord, "100")["details"], "ELDEN RING");

        // Both run for a moment; the game already shown stays.
        fx.start_proton_game(900, PORTAL);
        daemon.tick(fx.at(5), 6_000);
        assert_eq!(daemon.game().unwrap().app_id, ELDEN_RING);
        assert_eq!(discord.try_recv(Duration::from_millis(50)), None);

        // The old connection is cleared and a new one opened for the other
        // application. The two connections' frames may interleave.
        fx.stop_game(1000);
        daemon.tick(fx.at(10), 11_000);
        let frames = [discord.recv(), discord.recv(), discord.recv()];
        let handshake = json!({ "v": 1, "client_id": "200" });
        assert!(frames.contains(&(OP_HANDSHAKE, handshake)));
        let activities: Vec<Option<&Value>> = frames
            .iter()
            .filter(|(opcode, _)| *opcode == OP_FRAME)
            .map(|(_, body)| body["args"].get("activity"))
            .collect();
        assert_eq!(activities.len(), 2);
        assert!(activities.contains(&None), "old presence must be cleared");
        let portal = activities.iter().flatten().next().unwrap();
        assert_eq!(portal["details"], "Portal");
        assert_eq!(portal["timestamps"]["start"], 11_000);
    }

    #[test]
    fn waits_for_a_client_id_and_picks_up_config_changes() {
        let fx = Fixture::new(None);
        let discord = FakeDiscord::start(&fx.socket());
        fx.start_proton_game(1000, ELDEN_RING);
        let mut daemon = fx.daemon();

        daemon.tick(fx.at(0), 1_000);
        daemon.tick(fx.at(5), 6_000);
        assert_eq!(daemon.game().unwrap().name, "ELDEN RING");
        assert_eq!(daemon.connect_attempts(), 0);

        // A broken config is ignored rather than fatal.
        fs::write(&fx.paths.config_path, "default_client_id = [").unwrap();
        daemon.tick(fx.at(10), 11_000);
        assert_eq!(daemon.connect_attempts(), 0);

        fs::write(&fx.paths.config_path, "default_client_id = \"100\"\n").unwrap();
        daemon.tick(fx.at(15), 16_000);
        let activity = expect_presence(&discord, "100");
        assert_eq!(activity["timestamps"]["start"], 1_000);
    }

    #[test]
    fn keeps_retrying_after_a_rejected_handshake() {
        let fx = Fixture::new(Some("default_client_id = \"100\"\n"));
        let _discord = FakeDiscord::start_rejecting(&fx.socket());
        fx.start_proton_game(1000, ELDEN_RING);
        let mut daemon = fx.daemon();

        daemon.tick(fx.at(0), 0);
        daemon.tick(fx.at(5), 0);
        daemon.tick(fx.at(10), 0);
        assert!(!daemon.is_connected());
        assert_eq!(daemon.connect_attempts(), 2);
    }

    #[test]
    fn upgrades_to_proton_once_wine_processes_appear() {
        let fx = Fixture::new(Some("default_client_id = \"100\"\n"));
        let discord = FakeDiscord::start(&fx.socket());
        let mut daemon = fx.daemon();

        // First only a launch script is visible, without Wine variables.
        add_proc(
            &fx.paths.proc_root,
            990,
            "sh",
            &["sh", "-c", "launch"],
            &[("SteamAppId", "400")],
        );
        daemon.tick(fx.at(0), 1_000);
        assert_eq!(
            expect_presence(&discord, "100")["state"],
            "Playing on Steam"
        );

        fx.start_proton_game(1000, PORTAL);
        daemon.tick(fx.at(5), 6_000);
        let (_, body) = discord.recv();
        let activity = &body["args"]["activity"];
        assert_eq!(activity["state"], "Playing via Proton");
        assert_eq!(activity["timestamps"]["start"], 1_000);
    }
}
