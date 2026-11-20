/// Service definitions read straight from the files, without svmgr, so
/// that `enable`, `disable`, `is-enabled` and `list-unit-files` work in a
/// chroot or an image being built (with --root).
use std::path::{Path, PathBuf};

use dynamod_common::{config_dirs, paths};
use serde::Deserialize;

/// A service definition file after merging the vendor and admin directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitFile {
    /// The service name (`[service] name`), or the file stem for a mask.
    pub name: String,
    pub path: PathBuf,
    /// The definition's `enabled` field (default true).
    pub enabled_default: bool,
    /// The file name is masked: there is no definition.
    pub masked: bool,
}

#[derive(Deserialize)]
struct Head {
    service: HeadService,
}

#[derive(Deserialize)]
struct HeadService {
    name: String,
    #[serde(default = "yes")]
    enabled: bool,
}

fn yes() -> bool {
    true
}

/// All service definition files under `root`, sorted by file name.
/// Files that do not parse are left out (svmgr skips them too).
pub fn scan(root: Option<&Path>) -> Vec<UnitFile> {
    let mut out = Vec::new();
    for file in config_dirs::merge_dirs(&paths::service_dirs(root)) {
        if file.masked {
            let stem = file.file_name.trim_end_matches(".toml").to_string();
            out.push(UnitFile {
                name: stem,
                path: file.path,
                enabled_default: false,
                masked: true,
            });
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file.path) else {
            continue;
        };
        let Ok(head) = toml::from_str::<Head>(&text) else {
            continue;
        };
        out.push(UnitFile {
            name: head.service.name,
            path: file.path,
            enabled_default: head.service.enabled,
            masked: false,
        });
    }
    out
}
