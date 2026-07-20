use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

const MAX_STEM_CHARS: usize = 48;

pub fn next_export_path(source: &Path, page: usize, n_pages: usize) -> std::io::Result<PathBuf> {
    Ok(next_export_path_in_dir(
        source,
        page,
        n_pages,
        &std::env::current_dir()?,
    ))
}

fn next_export_path_in_dir(source: &Path, page: usize, n_pages: usize, dir: &Path) -> PathBuf {
    let width = n_pages.max(1).to_string().len();
    let stem = sanitized_stem(source);
    let base = format!("{stem}-page-{:0width$}", page.saturating_add(1));
    let mut candidate = dir.join(format!("{base}.png"));
    for suffix in 2.. {
        if !candidate.exists() {
            return candidate;
        }
        candidate = dir.join(format!("{base}-{suffix}.png"));
    }
    unreachable!()
}

fn sanitized_stem(source: &Path) -> String {
    let raw = source
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("document");
    let mut output = String::new();
    let mut dash = false;
    for character in raw.chars().take(MAX_STEM_CHARS) {
        let character = if character.is_ascii_alphanumeric() || matches!(character, '_' | '.') {
            character.to_ascii_lowercase()
        } else {
            '-'
        };
        if character == '-' {
            if !dash {
                let _ = output.write_char(character);
            }
            dash = true;
        } else {
            let _ = output.write_char(character);
            dash = false;
        }
    }
    let output = output.trim_matches(|character| matches!(character, '-' | '_' | '.'));
    if output.is_empty() {
        "document".to_owned()
    } else {
        output.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_name_is_sanitized_and_padded() {
        let path =
            next_export_path_in_dir(Path::new("Long Paper Title.pdf"), 2, 120, Path::new("/tmp"));
        assert_eq!(path.file_name().unwrap(), "long-paper-title-page-003.png");
    }
}
