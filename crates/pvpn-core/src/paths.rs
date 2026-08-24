//! Filesystem locations this tool cares about: Proton's cache, the
//! PYTHONPATH shim, and the system Python. Resolution prefers an explicit
//! env override, then an installed copy, then a git checkout, so `cargo run`
//! from the repo works without `setup.sh` having run.

use std::path::PathBuf;

/// Proton's cached logical-server list. `PVPN_SERVERLIST` overrides this,
/// which is how the CLI tests feed a fixture without touching the real cache.
pub fn serverlist_path() -> PathBuf {
    if let Ok(path) = std::env::var("PVPN_SERVERLIST") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache/Proton/VPN/serverlist.json")
}

/// Proton's own client log.
///
/// `pvpn` reads it to tell two failures apart that look identical from the
/// outside and want opposite responses: the network killed the tunnel
/// (blame the server, try another) versus our client certificate expired
/// (blame nothing, refresh the certificate — no server can work until it
/// is renewed). `PVPN_PROTON_LOG` overrides it for tests.
pub fn proton_log_path() -> PathBuf {
    if let Ok(path) = std::env::var("PVPN_PROTON_LOG") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache/Proton/VPN/logs/vpn-cli.log")
}

/// Directory put on `PYTHONPATH` so Proton's client loads our sitecustomize
/// and aiodns stubs. See `lib/sitecustomize.py`.
pub fn shim_dir() -> PathBuf {
    if let Ok(path) = std::env::var("PVPN_SHIM") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let installed = home.join(".local/share/pvpn");
    if installed.join("sitecustomize.py").exists() || installed.join("aiodns.py").exists() {
        return installed;
    }

    if let Some(found) = find_repo_lib() {
        return found;
    }

    let legacy = home.join(".local/share/protonvpn-torshim");
    if legacy.is_dir() {
        return legacy;
    }
    installed
}

/// Walk from the current binary and from cwd looking for `lib/sitecustomize.py`.
fn find_repo_lib() -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            roots.push(parent.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    for root in roots {
        for ancestor in root.ancestors() {
            let lib = ancestor.join("lib");
            if lib.join("sitecustomize.py").exists() {
                return Some(lib);
            }
        }
    }
    None
}

/// Always the distro Python. A conda/pyenv `python3` on PATH is the wrong
/// interpreter — Proton's packages are installed for `/usr/bin/python3`.
pub fn system_python() -> &'static str {
    "/usr/bin/python3"
}

/// Make sure the shim directory exists and has a stub `aiodns.py` so
/// torsocks can proxy DNS via getaddrinfo. Matches bash `need_shim`.
pub fn ensure_shim() -> anyhow::Result<PathBuf> {
    let shim = shim_dir();
    std::fs::create_dir_all(&shim)?;
    let aiodns = shim.join("aiodns.py");
    if !aiodns.exists() {
        std::fs::write(
            &aiodns,
            "raise ImportError(\"aiodns disabled so torsocks can proxy DNS via getaddrinfo\")\n",
        )?;
    }
    Ok(shim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serverlist_respects_env_override() {
        let prev = std::env::var_os("PVPN_SERVERLIST");
        std::env::set_var("PVPN_SERVERLIST", "/tmp/pvpn-fixture.json");
        assert_eq!(serverlist_path(), PathBuf::from("/tmp/pvpn-fixture.json"));
        match prev {
            Some(v) => std::env::set_var("PVPN_SERVERLIST", v),
            None => std::env::remove_var("PVPN_SERVERLIST"),
        }
    }
}
