//! `donsetch stop` : kill orphaned Chrome instances and clean up
//! stale lock files. Use after a crash or when Chrome processes
//! from a previous session are still resident.

pub fn run() {
    let profile = crate::ghost::profile_dir();

    #[cfg(unix)]
    {
        let pattern = format!("user-data-dir={}", profile.display());
        // pkill -9 -f matches the full command line. Kills every
        // Chrome process using the ghost profile, including renderers
        // and GPU processes that share the --user-data-dir argument.
        let out = std::process::Command::new("pkill")
            .args(["-9", "-f", pattern.as_str()])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                eprintln!("[ghost] killed orphaned Chrome instances");
            }
            Ok(_) => {
                // pkill exit 1 = no processes matched: not an error.
                eprintln!("[ghost] no orphaned Chrome instances found");
            }
            Err(_) => {
                eprintln!("[ghost] pkill not available, checking manually");
            }
        }
    }

    // Clean up stale lock files regardless of platform.
    for f in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let _ = std::fs::remove_file(profile.join(f));
    }
    let _ = std::fs::remove_file(crate::paths::cache_dir().join("ghost-profile.lock"));

    // Also clean up any temp profiles left behind by a crash.
    let temp_prefix = "donsetch-ghost-";
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(temp_prefix) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    eprintln!("[ghost] cleaned stale locks and temp profiles");
}
