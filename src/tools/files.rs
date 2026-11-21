//! `read_file`, `write_file`, `patch`, `search_files`.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

pub const READ_DEFAULT_LINES: usize = 500;
const READ_MAX_CHARS: usize = 100_000;
const SEARCH_MAX_RESULTS: usize = 100;

/// Resolve `p` against `workdir`, expanding a leading `~`.
pub fn resolve(workdir: &Path, p: &str) -> PathBuf {
    let p = p.trim();
    let expanded = if p == "~" {
        dirs::home_dir().unwrap_or_default()
    } else if let Some(rest) = p.strip_prefix("~/") {
        dirs::home_dir().unwrap_or_default().join(rest)
    } else {
        PathBuf::from(p)
    };
    if expanded.is_absolute() { expanded } else { workdir.join(expanded) }
}

pub fn read_file(workdir: &Path, path: &str, offset: Option<usize>, limit: Option<usize>) -> Result<String> {
    let full = resolve(workdir, path);
    let bytes = std::fs::read(&full).map_err(|e| anyhow::anyhow!("cannot read {}: {e}", full.display()))?;
    if bytes.iter().take(8192).any(|&b| b == 0) {
        bail!("{} looks like a binary file ({} bytes)", full.display(), bytes.len());
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let start = offset.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(READ_DEFAULT_LINES).max(1);
    if lines.is_empty() {
        return Ok(format!("{} is empty.", full.display()));
    }
    if start > lines.len() {
        bail!("offset {start} is past the end ({} lines)", lines.len());
    }
    let end = (start - 1 + limit).min(lines.len());
    let width = end.to_string().len();
    let mut out = String::new();
    for (i, l) in lines[start - 1..end].iter().enumerate() {
        out.push_str(&format!("{:>width$}|{}\n", start + i, l));
        if out.len() > READ_MAX_CHARS {
            out.push_str(&format!("[truncated at line {}; continue with offset={}]\n", start + i, start + i + 1));
            return Ok(out);
        }
    }
    if end < lines.len() {
        out.push_str(&format!("[showing lines {start}-{end} of {}; continue with offset={}]\n", lines.len(), end + 1));
    }
    Ok(out)
}

pub fn write_file(workdir: &Path, path: &str, content: &str) -> Result<String> {
    let full = resolve(workdir, path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existed = full.exists();
    std::fs::write(&full, content)?;
    Ok(format!(
        "{} {} ({} lines, {} bytes)",
        if existed { "Overwrote" } else { "Created" },
        full.display(),
        content.lines().count(),
        content.len()
    ))
}

/// Replace `old` with `new` in `text`. Exact match must be unique unless `replace_all`.
/// Falls back to a match that ignores leading/trailing whitespace per line, which is how
/// models most often get an edit slightly wrong.
pub fn replace_unique(text: &str, old: &str, new: &str, replace_all: bool) -> Result<String> {
    if old.is_empty() {
        bail!("old_string is empty");
    }
    let count = text.matches(old).count();
    if count == 1 || (replace_all && count > 0) {
        return Ok(if replace_all { text.replace(old, new) } else { text.replacen(old, new, 1) });
    }
    if count > 1 {
        bail!("old_string appears {count} times; add surrounding context to make it unique, or set replace_all");
    }
    // Whitespace-tolerant line-window match.
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let needle: Vec<&str> = old.lines().map(str::trim).collect();
    if needle.iter().all(|l| l.is_empty()) {
        bail!("old_string not found");
    }
    let n = needle.len();
    let windows: Vec<usize> =
        (0..lines.len().saturating_sub(n - 1)).filter(|&i| (0..n).all(|j| lines[i + j].trim() == needle[j])).collect();
    match windows.as_slice() {
        [i] => {
            let i = *i;
            let mut out: String = lines[..i].concat();
            out.push_str(new);
            let replaced_tail_newline = lines[i + n - 1].ends_with('\n');
            if replaced_tail_newline && !new.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&lines[i + n..].concat());
            Ok(out)
        }
        [] => bail!("old_string not found (also tried ignoring indentation). Re-read the file and copy the text exactly."),
        _ => bail!("old_string matches {} places when ignoring indentation; add more context", windows.len()),
    }
}

pub fn patch(workdir: &Path, path: &str, old: &str, new: &str, replace_all: bool) -> Result<String> {
    let full = resolve(workdir, path);
    let text = std::fs::read_to_string(&full).map_err(|e| anyhow::anyhow!("cannot read {}: {e}", full.display()))?;
    let next = replace_unique(&text, old, new, replace_all)?;
    std::fs::write(&full, &next)?;
    let delta = next.lines().count() as i64 - text.lines().count() as i64;
    Ok(format!("Patched {} ({delta:+} lines)", full.display()))
}

pub fn search_files(
    workdir: &Path,
    pattern: &str,
    path: Option<&str>,
    target: &str,
    file_glob: Option<&str>,
    limit: Option<usize>,
) -> Result<String> {
    let root = resolve(workdir, path.unwrap_or("."));
    let limit = limit.unwrap_or(SEARCH_MAX_RESULTS).clamp(1, 500);
    let mut builder = ignore::WalkBuilder::new(&root);
    builder.hidden(true).git_ignore(true).max_filesize(Some(5_000_000));
    let names_mode = target == "files";
    let glob_src = if names_mode { Some(pattern) } else { file_glob };
    if let Some(g) = glob_src.filter(|g| !g.is_empty()) {
        let mut ob = ignore::overrides::OverrideBuilder::new(&root);
        let g = if g.contains('/') || g.starts_with("**") { g.to_string() } else { format!("**/{g}") };
        ob.add(&g)?;
        builder.overrides(ob.build()?);
    }
    let re = if names_mode { None } else { Some(regex::Regex::new(pattern).map_err(|e| anyhow::anyhow!("bad regex: {e}"))?) };
    let mut out = Vec::new();
    let mut truncated = false;
    'walk: for entry in builder.build().flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let shown = p.strip_prefix(workdir).unwrap_or(p).display().to_string();
        match &re {
            None => {
                out.push(shown);
                if out.len() >= limit {
                    truncated = true;
                    break;
                }
            }
            Some(re) => {
                let Ok(bytes) = std::fs::read(p) else { continue };
                if bytes.iter().take(4096).any(|&b| b == 0) {
                    continue;
                }
                let text = String::from_utf8_lossy(&bytes);
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        let l: String = line.trim().chars().take(200).collect();
                        out.push(format!("{shown}:{}: {l}", i + 1));
                        if out.len() >= limit {
                            truncated = true;
                            break 'walk;
                        }
                    }
                }
            }
        }
    }
    if out.is_empty() {
        return Ok("No matches.".into());
    }
    if names_mode {
        out.sort();
    }
    let mut s = out.join("\n");
    if truncated {
        s.push_str(&format!("\n[stopped at {limit} results; narrow the search]"));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_with_offsets() {
        let d = tempfile::tempdir().unwrap();
        let body: String = (1..=12).map(|i| format!("line {i}\n")).collect();
        std::fs::write(d.path().join("a.txt"), body).unwrap();
        let r = read_file(d.path(), "a.txt", Some(10), Some(2)).unwrap();
        assert!(r.starts_with("10|line 10\n11|line 11\n"));
        assert!(r.contains("continue with offset=12"));
        assert!(read_file(d.path(), "a.txt", Some(99), None).is_err());
        std::fs::write(d.path().join("b.bin"), [0u8, 1, 2]).unwrap();
        assert!(read_file(d.path(), "b.bin", None, None).unwrap_err().to_string().contains("binary"));
    }

    #[test]
    fn write_creates_dirs() {
        let d = tempfile::tempdir().unwrap();
        assert!(write_file(d.path(), "x/y/z.md", "hi\n").unwrap().starts_with("Created"));
        assert!(write_file(d.path(), "x/y/z.md", "hi\n").unwrap().starts_with("Overwrote"));
        assert_eq!(std::fs::read_to_string(d.path().join("x/y/z.md")).unwrap(), "hi\n");
    }

    #[test]
    fn replace_semantics() {
        assert_eq!(replace_unique("a b c", "b", "B", false).unwrap(), "a B c");
        assert!(replace_unique("x x", "x", "y", false).unwrap_err().to_string().contains("2 times"));
        assert_eq!(replace_unique("x x", "x", "y", true).unwrap(), "y y");
        assert!(replace_unique("abc", "zzz", "y", false).is_err());
        // Indentation drift in the model's old_string still applies.
        let src = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";
        let out = replace_unique(src, "let a = 1;\n  let b = 2;", "    let a = 10;\n    let b = 20;", false).unwrap();
        assert_eq!(out, "fn main() {\n    let a = 10;\n    let b = 20;\n}\n");
    }

    #[test]
    fn search_content_and_names() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src/main.rs"), "fn main() {}\n// TODO: x\n").unwrap();
        std::fs::write(d.path().join("notes.md"), "TODO later\n").unwrap();
        std::fs::write(d.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(d.path().join("ignored")).unwrap();
        std::fs::write(d.path().join("ignored/x.rs"), "TODO hidden\n").unwrap();
        // ignore crate only honours .gitignore inside a git repo unless told otherwise
        std::fs::create_dir_all(d.path().join(".git")).unwrap();
        let r = search_files(d.path(), "TODO", None, "content", None, None).unwrap();
        assert!(r.contains("src/main.rs:2: // TODO: x"), "{r}");
        assert!(r.contains("notes.md:1"));
        assert!(!r.contains("hidden"));
        let r = search_files(d.path(), "TODO", None, "content", Some("*.rs"), None).unwrap();
        assert!(!r.contains("notes.md"));
        let r = search_files(d.path(), "*.rs", None, "files", None, None).unwrap();
        assert_eq!(r, "src/main.rs");
        assert!(search_files(d.path(), "(", None, "content", None, None).is_err());
        assert_eq!(search_files(d.path(), "nomatch_zz", None, "content", None, None).unwrap(), "No matches.");
    }
}
