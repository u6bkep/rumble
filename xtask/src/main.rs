use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::{self, Command},
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("lint-emoji") => lint_emoji(),
        Some("release-status") => release_status(),
        Some("--help" | "-h") | None => {
            eprintln!("Usage: cargo xtask <command>");
            eprintln!();
            eprintln!("Commands:");
            eprintln!("  lint-emoji      Check for emoji not renderable by the GUI fonts");
            eprintln!("  release-status  Show which crates changed since the last vX.Y.Z tag");
            eprintln!("                  and flag any that changed but were not version-bumped");
        }
        Some(cmd) => {
            eprintln!("Unknown command: {cmd}");
            eprintln!("Run `cargo xtask --help` for available commands.");
            process::exit(1);
        }
    }
}

/// Per-crate release bookkeeping (see RELEASING.md). Releases are cut by
/// pushing an umbrella `vX.Y.Z` tag, but each crate's `Cargo.toml` version is
/// bumped independently as it changes. This command diffs every crate against
/// the last release tag and reports three states:
///
///   - `unchanged`   no files touched since the tag → leave the version alone
///   - `bumped`      changed AND version already moved since the tag → good
///   - `NEEDS BUMP`  changed but version is still what it was at the tag
///   - `new`         crate did not exist at the tag
///
/// Exits non-zero if any crate is in the NEEDS BUMP state, so it can gate a
/// release: run it before tagging and bump anything it flags.
fn release_status() {
    let root = workspace_root();

    let tag = match last_release_tag() {
        Some(t) => t,
        None => {
            eprintln!("No vX.Y.Z release tag found in history; nothing to compare against.");
            process::exit(1);
        }
    };
    eprintln!("Comparing crates against last release tag: {tag}\n");

    let crates_dir = root.join("crates");
    let mut names: Vec<String> = fs::read_dir(&crates_dir)
        .unwrap_or_else(|e| {
            eprintln!("Failed to read {}: {e}", crates_dir.display());
            process::exit(1);
        })
        .flatten()
        .filter(|e| e.path().join("Cargo.toml").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();

    let mut needs_bump = Vec::new();

    for name in &names {
        let rel_path = format!("crates/{name}");
        let changed = git_changed_file_count(&root, &tag, &rel_path);
        let cur_ver = crate_version(&crates_dir.join(name).join("Cargo.toml"));
        let tag_ver = git_show_crate_version(&root, &tag, name);

        let (status, ver_note) = match (changed, &tag_ver) {
            (0, _) => ("unchanged", cur_ver.clone().unwrap_or_default()),
            (_, None) => ("new", cur_ver.clone().unwrap_or_default()),
            (_, Some(old)) if Some(old) != cur_ver.as_ref() => {
                ("bumped", format!("{old} -> {}", cur_ver.clone().unwrap_or_default()))
            }
            (_, Some(old)) => {
                needs_bump.push(name.clone());
                ("NEEDS BUMP", old.clone())
            }
        };

        let files = if changed == 0 {
            String::new()
        } else {
            format!("{changed} file(s) changed")
        };
        println!("  {name:<24} {status:<11} {ver_note:<16} {files}");
    }

    if needs_bump.is_empty() {
        eprintln!("\nAll changed crates have been version-bumped since {tag}.");
    } else {
        eprintln!(
            "\n{} crate(s) changed without a version bump: {}",
            needs_bump.len(),
            needs_bump.join(", ")
        );
        eprintln!("Bump each crate's `version` in its Cargo.toml before tagging the release.");
        process::exit(1);
    }
}

/// The most recent `vX.Y.Z`-style tag reachable from HEAD.
fn last_release_tag() -> Option<String> {
    let out = Command::new("git")
        .args(["describe", "--tags", "--abbrev=0", "--match", "v[0-9]*"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let tag = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!tag.is_empty()).then_some(tag)
}

/// Number of files under `rel_path` that differ between `tag` and HEAD.
fn git_changed_file_count(root: &Path, tag: &str, rel_path: &str) -> usize {
    let out = Command::new("git")
        .current_dir(root)
        .args(["diff", "--name-only", &format!("{tag}..HEAD"), "--", rel_path])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .count(),
        _ => 0,
    }
}

/// The crate's `version` as recorded in `Cargo.toml` at the given `tag`, or
/// `None` if the crate did not exist there.
fn git_show_crate_version(root: &Path, tag: &str, name: &str) -> Option<String> {
    let out = Command::new("git")
        .current_dir(root)
        .arg("show")
        .arg(format!("{tag}:crates/{name}/Cargo.toml"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_version(&String::from_utf8_lossy(&out.stdout))
}

/// Read the `[package] version` from a Cargo.toml on disk.
fn crate_version(manifest: &Path) -> Option<String> {
    parse_version(&fs::read_to_string(manifest).ok()?)
}

/// Pull the first top-level `version = "..."` line out of a Cargo.toml's text.
/// Anchored to column 0 so it matches the package version, not a dependency's.
fn parse_version(manifest: &str) -> Option<String> {
    manifest
        .lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .map(str::to_string)
}

fn lint_emoji() {
    let root = workspace_root();
    let emoji_doc = root.join("docs/supported_emoji.md");

    let allowlist = match fs::read_to_string(&emoji_doc) {
        Ok(content) => parse_allowlist(&content),
        Err(e) => {
            eprintln!("Failed to read {}: {e}", emoji_doc.display());
            process::exit(1);
        }
    };

    eprintln!("Loaded {} allowed codepoints from supported_emoji.md", allowlist.len());

    let mut violations = Vec::new();
    let crates_dir = root.join("crates");

    walk_rs_files(&crates_dir, &mut |path| {
        scan_file(path, &allowlist, &mut violations);
    });

    if violations.is_empty() {
        println!("No unsupported emoji found.");
    } else {
        for v in &violations {
            let rel = v.file.strip_prefix(&root).unwrap_or(&v.file);
            println!(
                "{}:{}:{}: unsupported '{}' (U+{:04X}) - not in supported_emoji.md",
                rel.display(),
                v.line,
                v.col,
                v.ch,
                v.ch as u32,
            );
        }
        let unique: HashSet<char> = violations.iter().map(|v| v.ch).collect();
        eprintln!(
            "\nFound {} unique unsupported characters across {} locations",
            unique.len(),
            violations.len()
        );
        process::exit(1);
    }
}

fn workspace_root() -> PathBuf {
    let start = std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap());

    let mut dir = start.as_path();
    loop {
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.exists()
            && let Ok(content) = fs::read_to_string(&cargo_toml)
            && content.contains("[workspace]")
        {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => {
                eprintln!("Could not find workspace root");
                process::exit(1);
            }
        }
    }
}

/// Parse all `U+XXXX` codepoints from the supported_emoji.md document.
fn parse_allowlist(content: &str) -> HashSet<u32> {
    let mut set = HashSet::new();
    for line in content.lines() {
        if let Some(idx) = line.find("U+") {
            let hex_start = idx + 2;
            let hex_str: String = line[hex_start..]
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .collect();
            if !hex_str.is_empty()
                && let Ok(cp) = u32::from_str_radix(&hex_str, 16)
            {
                set.insert(cp);
            }
        }
    }
    set
}

/// Returns true if the character is in a Unicode block where emoji/pictographic
/// symbols live and needs to be in the font allowlist to render correctly.
///
/// This intentionally excludes "text symbol" ranges (math operators, currency,
/// box drawing, basic arrows) since those render through the text fonts
/// (Ubuntu-Light, Hack) without needing the emoji fonts.
fn is_emoji_candidate(c: char) -> bool {
    let cp = c as u32;
    matches!(
        cp,
        0x203C | 0x2049
            | 0x2122
            | 0x2139
            | 0x2194..=0x21AA
            | 0x231A..=0x23FF
            | 0x24C2
            | 0x25A0..=0x25FF
            | 0x2600..=0x27BF
            | 0x27F2..=0x27F3
            | 0x2934..=0x2935
            | 0x2B05..=0x2BFF
            | 0x3030
            | 0x303D
            | 0x3297
            | 0x3299
            | 0x1D11E
            | 0x1F000..=0x1FFFF
    )
}

struct Violation {
    file: PathBuf,
    line: usize,
    col: usize,
    ch: char,
}

fn scan_file(path: &Path, allowlist: &HashSet<u32>, violations: &mut Vec<Violation>) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };

    for (line_idx, line) in content.lines().enumerate() {
        for (byte_col, ch) in line.char_indices() {
            if is_emoji_candidate(ch) && !allowlist.contains(&(ch as u32)) {
                violations.push(Violation {
                    file: path.to_path_buf(),
                    line: line_idx + 1,
                    col: byte_col + 1,
                    ch,
                });
            }
        }
    }
}

fn walk_rs_files(dir: &Path, callback: &mut dyn FnMut(&Path)) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        if name_str == "target" || name_str.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            walk_rs_files(&path, callback);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            callback(&path);
        }
    }
}
