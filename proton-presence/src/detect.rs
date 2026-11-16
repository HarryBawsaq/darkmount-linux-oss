//! Detects running Steam games by scanning process environments in `/proc`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// A Steam app with at least one live process that is not launcher plumbing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningGame {
    pub app_id: u64,
    /// Lowest PID among the app's game processes.
    pub pid: u32,
    /// Whether any process of the app runs under Wine, i.e. through Proton.
    pub proton: bool,
}

/// Process names that belong to Steam, the Steam Runtime container, or Wine
/// itself. They carry the game's app ID but outlive or precede the game.
const INFRASTRUCTURE_NAMES: &[&str] = &[
    // Steam launch chain
    "reaper",
    "steam",
    "steamwebhelper",
    "gameoverlayui",
    "proton",
    "_v2-entry-point",
    "bwrap",
    "srt-bwrap",
    // Wine services that linger after the game exits
    "services.exe",
    "winedevice.exe",
    "plugplay.exe",
    "svchost.exe",
    "rpcss.exe",
    "explorer.exe",
    "tabtip.exe",
    "conhost.exe",
    "start.exe",
    "wineboot.exe",
    "winemenubuilder.exe",
    "xalia.exe",
    // Proton's Steam client shim
    "steam.exe",
];

/// Prefixes instead of full names, because `/proc/<pid>/comm` is cut to 15
/// characters (`pressure-vessel-wrap` shows up as `pressure-vessel`).
const INFRASTRUCTURE_PREFIXES: &[&str] = &[
    "wineserver",
    "steam-runtime",
    "steam-launch-wr",
    "pressure-vessel",
    "pv-",
];

/// Environment variables that only Wine processes carry.
const WINE_MARKERS: &[&[u8]] = &[b"WINEPREFIX", b"WINEDLLPATH", b"WINELOADER"];

/// The app ID and Wine marker found in a `/proc/<pid>/environ` blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SteamEnv {
    pub app_id: u64,
    pub wine: bool,
}

/// Reads the Steam app ID out of a NUL-separated environment block.
///
/// `SteamAppId` wins when both are set; `SteamGameId` covers non-Steam
/// shortcuts, where `SteamAppId` is 0 or absent.
pub fn parse_environ(environ: &[u8]) -> Option<SteamEnv> {
    let mut app_id = None;
    let mut game_id = None;
    let mut wine = false;

    for entry in environ.split(|&b| b == 0) {
        let Some(eq) = entry.iter().position(|&b| b == b'=') else {
            continue;
        };
        let (key, value) = (&entry[..eq], &entry[eq + 1..]);
        let id = || {
            std::str::from_utf8(value)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|&id| id != 0)
        };
        match key {
            b"SteamAppId" => app_id = id(),
            b"SteamGameId" => game_id = id(),
            _ if WINE_MARKERS.contains(&key) => wine = true,
            _ => {}
        }
    }

    Some(SteamEnv {
        app_id: app_id.or(game_id)?,
        wine,
    })
}

/// Last path component of a Unix or Windows path, lowercased.
fn base_name(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase()
}

fn is_infrastructure_name(name: &str) -> bool {
    INFRASTRUCTURE_NAMES.contains(&name)
        || INFRASTRUCTURE_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Whether a process is launcher, container or Wine plumbing rather than
/// the game. `comm` and `cmdline` are the contents of the `/proc` files.
pub fn is_infrastructure(comm: &str, cmdline: &[u8]) -> bool {
    let mut args = cmdline
        .split(|&b| b == 0)
        .map(|arg| base_name(&String::from_utf8_lossy(arg)));
    let arg0 = args.next().unwrap_or_default();

    if is_infrastructure_name(&base_name(comm.trim())) || is_infrastructure_name(&arg0) {
        return true;
    }
    // The Proton launcher is a Python script: `python3 .../proton waitforexitandrun ...`.
    arg0.starts_with("python") && args.next().as_deref() == Some("proton")
}

/// Scans a `/proc`-style directory and returns the running games, ordered
/// by app ID. Unreadable processes are skipped.
pub fn scan(proc_root: &Path) -> Vec<RunningGame> {
    struct App {
        game_pid: Option<u32>,
        wine: bool,
    }
    let mut apps: BTreeMap<u64, App> = BTreeMap::new();

    let Ok(entries) = fs::read_dir(proc_root) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let dir = entry.path();
        let Some(env) = fs::read(dir.join("environ"))
            .ok()
            .and_then(|bytes| parse_environ(&bytes))
        else {
            continue;
        };

        let app = apps.entry(env.app_id).or_insert(App {
            game_pid: None,
            wine: false,
        });
        app.wine |= env.wine;

        let comm = fs::read_to_string(dir.join("comm")).unwrap_or_default();
        let cmdline = fs::read(dir.join("cmdline")).unwrap_or_default();
        if !is_infrastructure(&comm, &cmdline) {
            app.game_pid = Some(app.game_pid.map_or(pid, |lowest| lowest.min(pid)));
        }
    }

    apps.into_iter()
        .filter_map(|(app_id, app)| {
            Some(RunningGame {
                app_id,
                pid: app.game_pid?,
                proton: app.wine,
            })
        })
        .collect()
}

/// Chooses which game to show. The game already being shown stays as long
/// as it is running; otherwise the one with the lowest PID wins.
pub fn pick_active(games: &[RunningGame], current: Option<u64>) -> Option<&RunningGame> {
    games
        .iter()
        .find(|g| Some(g.app_id) == current)
        .or_else(|| games.iter().min_by_key(|g| g.pid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{add_proc, TempDir};

    fn environ(vars: &[(&str, &str)]) -> Vec<u8> {
        vars.iter()
            .flat_map(|(k, v)| format!("{k}={v}\0").into_bytes())
            .collect()
    }

    #[test]
    fn parses_app_id_from_environment() {
        let env = environ(&[("HOME", "/home/u"), ("SteamAppId", "620"), ("TERM", "x")]);
        assert_eq!(
            parse_environ(&env),
            Some(SteamEnv {
                app_id: 620,
                wine: false
            })
        );
        assert_eq!(parse_environ(&environ(&[("HOME", "/home/u")])), None);
        assert_eq!(parse_environ(b""), None);
    }

    #[test]
    fn steam_game_id_is_a_fallback_for_app_id() {
        let only_game_id = environ(&[("SteamGameId", "1245620")]);
        assert_eq!(parse_environ(&only_game_id).unwrap().app_id, 1245620);

        let both = environ(&[("SteamGameId", "111"), ("SteamAppId", "222")]);
        assert_eq!(parse_environ(&both).unwrap().app_id, 222);

        // Non-Steam shortcut: app ID 0, 64-bit game ID.
        let shortcut = environ(&[("SteamAppId", "0"), ("SteamGameId", "12345678901234567890")]);
        assert_eq!(
            parse_environ(&shortcut).unwrap().app_id,
            12345678901234567890
        );
    }

    #[test]
    fn ignores_invalid_ids_and_lookalike_variables() {
        assert_eq!(parse_environ(&environ(&[("SteamAppId", "abc")])), None);
        assert_eq!(parse_environ(&environ(&[("SteamAppId", "")])), None);
        assert_eq!(parse_environ(&environ(&[("SteamAppId", "0")])), None);
        assert_eq!(parse_environ(&environ(&[("XSteamAppId", "5")])), None);
        assert_eq!(parse_environ(&environ(&[("SteamAppIdX", "5")])), None);
        assert_eq!(parse_environ(b"SteamAppId\0\xff\xfe=1\0"), None);
    }

    #[test]
    fn detects_wine_environment() {
        let env = environ(&[("SteamAppId", "5"), ("WINEPREFIX", "/x/pfx")]);
        assert!(parse_environ(&env).unwrap().wine);
        let env = environ(&[("SteamAppId", "5"), ("NOT_WINEPREFIX", "1")]);
        assert!(!parse_environ(&env).unwrap().wine);
    }

    #[test]
    fn recognises_infrastructure_processes() {
        let infra: &[(&str, &[u8])] = &[
            (
                "reaper",
                b"/home/u/.steam/ubuntu12_32/reaper\0SteamLaunch\0AppId=620\0",
            ),
            ("wineserver", b"/x/files/bin/wineserver\0"),
            (
                "steam-runtime-l",
                b"/x/steam-runtime-launcher-service\0--alongside-steam\0",
            ),
            (
                "pressure-vessel",
                b"/x/pressure-vessel/bin/pressure-vessel-wrap\0",
            ),
            ("pv-bwrap", b"/x/pressure-vessel/libexec/pv-bwrap\0"),
            (
                "pv-adverb",
                b"/usr/lib/pressure-vessel/from-host/bin/pressure-vessel-adverb\0",
            ),
            (
                "steam-launch-wr",
                b"/x/ubuntu12_32/steam-launch-wrapper\0--\0",
            ),
            (
                "python3",
                b"python3\0/x/Proton/proton\0waitforexitandrun\0game.exe\0",
            ),
            ("services.exe", b"C:\\windows\\system32\\services.exe\0"),
            ("winedevice.exe", b"C:\\windows\\system32\\winedevice.exe\0"),
            (
                "steam.exe",
                b"c:\\windows\\system32\\steam.exe\0Z:\\games\\game.exe\0",
            ),
            (
                "Explorer.EXE",
                b"C:\\windows\\system32\\explorer.exe\0/desktop\0",
            ),
        ];
        for (comm, cmdline) in infra {
            assert!(is_infrastructure(comm, cmdline), "{comm} should be ignored");
        }

        let games: &[(&str, &[u8])] = &[
            (
                "eldenring.exe",
                b"Z:\\games\\ELDEN RING\\Game\\eldenring.exe\0",
            ),
            (
                "portal2_linux",
                b"/games/Portal 2/portal2_linux\0-game\0portal2\0",
            ),
            (
                "wine64-preloade",
                b"/x/bin/wine64-preloader\0/x/bin/wine64\0game.exe\0",
            ),
            ("python3", b"python3\0/games/renpy/game.py\0"),
            ("Main Thread", b""),
        ];
        for (comm, cmdline) in games {
            assert!(!is_infrastructure(comm, cmdline), "{comm} should count");
        }
    }

    #[test]
    fn scan_finds_proton_game_and_ignores_infrastructure() {
        let tmp = TempDir::new("proc");
        let proc_root = tmp.path();
        let steam = [("SteamAppId", "1245620"), ("SteamGameId", "1245620")];
        let wine = [
            ("SteamAppId", "1245620"),
            ("SteamGameId", "1245620"),
            ("WINEPREFIX", "/x/compatdata/1245620/pfx"),
        ];

        add_proc(proc_root, 1, "systemd", &["/sbin/init"], &[("HOME", "/")]);
        add_proc(proc_root, 900, "bash", &["bash"], &[("HOME", "/home/u")]);
        add_proc(
            proc_root,
            1000,
            "reaper",
            &["reaper", "SteamLaunch"],
            &steam,
        );
        add_proc(proc_root, 1001, "pv-bwrap", &["/x/pv-bwrap"], &steam);
        add_proc(proc_root, 1002, "wineserver", &["/x/wineserver"], &wine);
        add_proc(
            proc_root,
            1003,
            "services.exe",
            &["C:\\windows\\system32\\services.exe"],
            &wine,
        );
        add_proc(
            proc_root,
            1010,
            "eldenring.exe",
            &["Z:\\g\\eldenring.exe"],
            &wine,
        );
        add_proc(
            proc_root,
            1011,
            "eldenring.exe",
            &["Z:\\g\\eldenring.exe"],
            &wine,
        );
        // Entries a real /proc also contains.
        fs::create_dir_all(proc_root.join("self")).unwrap();
        fs::create_dir_all(proc_root.join("2000")).unwrap();
        fs::write(proc_root.join("uptime"), "1 1").unwrap();

        assert_eq!(
            scan(proc_root),
            [RunningGame {
                app_id: 1245620,
                pid: 1010,
                proton: true
            }]
        );
    }

    #[test]
    fn scan_reports_nothing_when_only_infrastructure_remains() {
        let tmp = TempDir::new("proc");
        let env = [("SteamAppId", "620"), ("WINEPREFIX", "/x/pfx")];
        add_proc(tmp.path(), 10, "reaper", &["reaper", "SteamLaunch"], &env);
        add_proc(tmp.path(), 11, "wineserver", &["wineserver"], &env);
        add_proc(
            tmp.path(),
            12,
            "steam-runtime-l",
            &["steam-runtime-launcher-service"],
            &env,
        );
        add_proc(
            tmp.path(),
            13,
            "pressure-vessel",
            &["pressure-vessel-wrap"],
            &env,
        );
        assert!(scan(tmp.path()).is_empty());
    }

    #[test]
    fn scan_marks_native_games_as_not_proton() {
        let tmp = TempDir::new("proc");
        add_proc(
            tmp.path(),
            50,
            "portal2_linux",
            &["./portal2_linux"],
            &[("SteamAppId", "620")],
        );
        assert_eq!(
            scan(tmp.path()),
            [RunningGame {
                app_id: 620,
                pid: 50,
                proton: false
            }]
        );
    }

    #[test]
    fn scan_of_missing_or_empty_proc_is_empty() {
        let tmp = TempDir::new("proc");
        assert!(scan(tmp.path()).is_empty());
        assert!(scan(&tmp.path().join("does-not-exist")).is_empty());
    }

    #[test]
    fn active_game_is_sticky_then_lowest_pid() {
        let game = |app_id, pid| RunningGame {
            app_id,
            pid,
            proton: true,
        };
        let games = [game(10, 500), game(20, 300)];
        assert_eq!(pick_active(&games, None).unwrap().app_id, 20);
        assert_eq!(pick_active(&games, Some(10)).unwrap().app_id, 10);
        assert_eq!(pick_active(&games, Some(99)).unwrap().app_id, 20);
        assert_eq!(pick_active(&[], Some(10)), None);
    }
}
