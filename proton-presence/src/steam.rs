//! Steam library discovery and app ID to name resolution.

use std::fs;
use std::path::{Path, PathBuf};

use crate::vdf::{self, Value};

/// Steam installation roots relative to the home directory: the native
/// client and the Flatpak (`com.valvesoftware.Steam`) client.
const STEAM_ROOTS: &[&str] = &[
    ".steam/steam",
    ".steam/root",
    ".local/share/Steam",
    ".var/app/com.valvesoftware.Steam/.steam/steam",
    ".var/app/com.valvesoftware.Steam/.local/share/Steam",
    ".var/app/com.valvesoftware.Steam/data/Steam",
];

/// Extracts library paths from the text of a `libraryfolders.vdf`.
///
/// Handles the current layout (`"0" { "path" "/x" }`) and the older one
/// (`"1" "/x"`).
pub fn library_paths_from_vdf(text: &str) -> Vec<PathBuf> {
    let Ok(doc) = vdf::parse(text) else {
        return Vec::new();
    };
    let Some(folders) = doc.get("libraryfolders") else {
        return Vec::new();
    };
    folders
        .entries()
        .iter()
        .filter(|(key, _)| !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|(_, value)| match value {
            Value::Str(path) => Some(path.as_str()),
            obj => obj.get("path").and_then(Value::as_str),
        })
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Extracts the display name from the text of an `appmanifest_<id>.acf`.
pub fn app_name_from_acf(text: &str) -> Option<String> {
    let doc = vdf::parse(text).ok()?;
    let name = doc.get("AppState")?.get("name")?.as_str()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Every Steam library directory (the ones containing `steamapps/`) that
/// exists under `home`, without duplicates.
pub fn library_dirs(home: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for root in STEAM_ROOTS.iter().map(|rel| home.join(rel)) {
        for index in ["steamapps/libraryfolders.vdf", "config/libraryfolders.vdf"] {
            if let Ok(text) = fs::read_to_string(root.join(index)) {
                candidates.extend(library_paths_from_vdf(&text));
            }
        }
        candidates.push(root);
    }

    let mut seen = Vec::new();
    let mut dirs = Vec::new();
    for dir in candidates {
        if !dir.join("steamapps").is_dir() {
            continue;
        }
        // `~/.steam/steam` is normally a symlink to `~/.local/share/Steam`.
        let canonical = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !seen.contains(&canonical) {
            seen.push(canonical);
            dirs.push(dir);
        }
    }
    dirs
}

/// Resolves a Steam app ID to its display name from the installed manifests.
pub fn app_name(home: &Path, app_id: u64) -> Option<String> {
    library_dirs(home).into_iter().find_map(|dir| {
        let manifest = dir
            .join("steamapps")
            .join(format!("appmanifest_{app_id}.acf"));
        app_name_from_acf(&fs::read_to_string(manifest).ok()?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    const MANIFEST: &str = r#"
"AppState"
{
	"appid"		"1245620"
	"Universe"		"1"
	"name"		"ELDEN RING"
	"StateFlags"		"4"
	"installdir"		"ELDEN RING"
	"UserConfig"
	{
		"language"		"english"
	}
}
"#;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn reads_name_from_app_manifest() {
        assert_eq!(app_name_from_acf(MANIFEST).as_deref(), Some("ELDEN RING"));
        assert_eq!(app_name_from_acf(r#""AppState" { "appid" "1" }"#), None);
        assert_eq!(app_name_from_acf(r#""AppState" { "name" "" }"#), None);
        assert_eq!(app_name_from_acf("not vdf {"), None);
    }

    #[test]
    fn reads_library_paths_in_current_format() {
        let text = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"/home/u/.local/share/Steam"
		"label"		""
		"apps"
		{
			"228980"		"460372343"
		}
	}
	"1"
	{
		"path"		"/mnt/games/SteamLibrary"
	}
}
"#;
        assert_eq!(
            library_paths_from_vdf(text),
            [
                PathBuf::from("/home/u/.local/share/Steam"),
                PathBuf::from("/mnt/games/SteamLibrary"),
            ]
        );
    }

    #[test]
    fn reads_library_paths_in_legacy_format() {
        let text = r#"
"LibraryFolders"
{
	"TimeNextStatsReport"		"1600000000"
	"ContentStatsID"		"-1"
	"1"		"/mnt/old/SteamLibrary"
}
"#;
        assert_eq!(
            library_paths_from_vdf(text),
            [PathBuf::from("/mnt/old/SteamLibrary")]
        );
        assert!(library_paths_from_vdf("garbage {").is_empty());
    }

    #[test]
    fn resolves_names_across_libraries() {
        let tmp = TempDir::new("steam");
        let home = tmp.path().join("home");
        let main = home.join(".local/share/Steam");
        let extra = tmp.path().join("extra");
        let flatpak = home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam");

        write(
            &main.join("steamapps/libraryfolders.vdf"),
            &format!(
                r#""libraryfolders" {{ "0" {{ "path" "{}" }} "1" {{ "path" "{}" }} }}"#,
                main.display(),
                extra.display()
            ),
        );
        write(&main.join("steamapps/appmanifest_1245620.acf"), MANIFEST);
        write(
            &extra.join("steamapps/appmanifest_620.acf"),
            r#""AppState" { "appid" "620" "name" "Portal 2" }"#,
        );
        write(
            &flatpak.join("steamapps/appmanifest_400.acf"),
            r#""AppState" { "appid" "400" "name" "Portal" }"#,
        );
        // Mimic the usual ~/.steam/steam symlink; it must not add a duplicate.
        fs::create_dir_all(home.join(".steam")).unwrap();
        std::os::unix::fs::symlink(&main, home.join(".steam/steam")).unwrap();

        assert_eq!(library_dirs(&home).len(), 3);
        assert_eq!(app_name(&home, 1245620).as_deref(), Some("ELDEN RING"));
        assert_eq!(app_name(&home, 620).as_deref(), Some("Portal 2"));
        assert_eq!(app_name(&home, 400).as_deref(), Some("Portal"));
        assert_eq!(app_name(&home, 999), None);
    }

    #[test]
    fn missing_steam_install_yields_nothing() {
        let tmp = TempDir::new("nosteam");
        assert!(library_dirs(tmp.path()).is_empty());
        assert_eq!(app_name(tmp.path(), 1), None);
    }
}
