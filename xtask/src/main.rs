use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("lint-emoji") => lint_emoji(),
        Some("--help" | "-h") | None => {
            eprintln!("Usage: cargo xtask <command>");
            eprintln!();
            eprintln!("Commands:");
            eprintln!("  lint-emoji  Check for emoji not renderable by egui fonts");
        }
        Some(cmd) => {
            eprintln!("Unknown command: {cmd}");
            eprintln!("Run `cargo xtask --help` for available commands.");
            process::exit(1);
        }
    }
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
        if cargo_toml.exists() {
            if let Ok(content) = fs::read_to_string(&cargo_toml) {
                if content.contains("[workspace]") {
                    return dir.to_path_buf();
                }
            }
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
            if !hex_str.is_empty() {
                if let Ok(cp) = u32::from_str_radix(&hex_str, 16) {
                    set.insert(cp);
                }
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
