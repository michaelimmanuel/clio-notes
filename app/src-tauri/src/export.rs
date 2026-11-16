//! Saving a meeting's final notes outside the meeting folder.
//!
//! The user picks the file in a save dialog (in the UI); these commands write
//! to that path:
//!
//! - **Markdown:** `<name>.md`, plus `<name> images/` next to it with the
//!   screenshots the notes use, linked by relative paths.
//! - **Zip:** one `<name>.zip` holding `notes.md`, `images/` and
//!   `transcript.md`, for email or Notion's own Markdown import.

use std::io::Write;
use std::path::{Path, PathBuf};

use pipeline::final_pass::{self, FinalNotes, MeetingInput};
use serde::Deserialize;
use storage::Timeline;

use crate::meetings::meeting_dir;
use crate::settings::UiError;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveFormat {
    Markdown,
    Zip,
}

/// The final notes and what they were written from; an error when the
/// meeting has no final notes yet.
pub(crate) fn final_notes(dir: &Path) -> Result<(FinalNotes, MeetingInput), UiError> {
    let timeline = Timeline::open(&dir.join("timeline.sqlite"))
        .map_err(|e| UiError::new("storage", e.to_string()))?;
    let notes = final_pass::stored(&timeline).ok_or_else(|| {
        UiError::new(
            "not_found",
            "This meeting has no final notes yet. Write them first.",
        )
    })?;
    drop(timeline);
    let input = final_pass::load(dir).map_err(|e| UiError::new("storage", e))?;
    Ok((notes, input))
}

/// The image files the notes use, relative to the meeting folder, once each.
pub(crate) fn used_images(notes: &FinalNotes, dir: &Path) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for t in &notes.topics {
        if let Some(image) = &t.image
            && !image.file.is_empty()
            && !files.contains(&image.file)
            && dir.join(&image.file).is_file()
        {
            files.push(image.file.clone());
        }
    }
    files
}

fn io_error(path: &Path, e: impl std::fmt::Display) -> UiError {
    UiError::new("storage", format!("Cannot write {}: {e}", path.display()))
}

/// `path` with `ext` as its extension, unless it already has it.
fn with_extension(path: &str, ext: &str) -> PathBuf {
    let path = PathBuf::from(path);
    let has = path
        .extension()
        .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case(ext));
    if has { path } else { path.with_extension(ext) }
}

fn save_markdown(dir: &Path, target: &Path) -> Result<PathBuf, UiError> {
    let (notes, input) = final_notes(dir)?;
    let stem = target
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "notes".into());
    let parent = target.parent().unwrap_or(Path::new("."));
    let image_dir = format!("{stem} images");
    let images = used_images(&notes, dir);
    if !images.is_empty() {
        let out = parent.join(&image_dir);
        std::fs::create_dir_all(&out).map_err(|e| io_error(&out, e))?;
        for file in &images {
            let name = file.strip_prefix("images/").unwrap_or(file);
            let to = out.join(name);
            std::fs::copy(dir.join(file), &to).map_err(|e| io_error(&to, e))?;
        }
    }
    let md = final_pass::markdown_standalone(&notes, &input, &image_dir);
    std::fs::write(target, md).map_err(|e| io_error(target, e))?;
    Ok(target.to_path_buf())
}

fn save_zip(dir: &Path, target: &Path) -> Result<PathBuf, UiError> {
    let (notes, input) = final_notes(dir)?;
    let file = std::fs::File::create(target).map_err(|e| io_error(target, e))?;
    let mut zip = zip::ZipWriter::new(file);
    let text = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    // PNG is already compressed.
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let err = |e: &dyn std::fmt::Display| io_error(target, e);

    zip.start_file("notes.md", text).map_err(|e| err(&e))?;
    zip.write_all(final_pass::markdown_standalone(&notes, &input, "images").as_bytes())
        .map_err(|e| err(&e))?;
    zip.start_file("transcript.md", text).map_err(|e| err(&e))?;
    zip.write_all(final_pass::transcript_markdown(&input).as_bytes())
        .map_err(|e| err(&e))?;
    for image in used_images(&notes, dir) {
        let bytes = std::fs::read(dir.join(&image)).map_err(|e| err(&e))?;
        zip.start_file(image.as_str(), stored)
            .map_err(|e| err(&e))?;
        zip.write_all(&bytes).map_err(|e| err(&e))?;
    }
    zip.finish().map_err(|e| err(&e))?;
    Ok(target.to_path_buf())
}

/// Saves the final notes of meeting `id` to `path`, chosen by the user.
/// Returns the path written, which may have gained an extension.
#[tauri::command]
pub async fn save_notes(id: String, path: String, format: SaveFormat) -> Result<String, UiError> {
    let dir = meeting_dir(&id)?;
    tauri::async_runtime::spawn_blocking(move || {
        let written = match format {
            SaveFormat::Markdown => save_markdown(&dir, &with_extension(&path, "md")),
            SaveFormat::Zip => save_zip(&dir, &with_extension(&path, "zip")),
        }?;
        log::info!("notes saved as {format:?}");
        Ok(written.display().to_string())
    })
    .await
    .map_err(|e| UiError::new("storage", e.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_is_added_once() {
        assert_eq!(with_extension("a/notes", "md"), PathBuf::from("a/notes.md"));
        assert_eq!(
            with_extension("a/notes.MD", "md"),
            PathBuf::from("a/notes.MD")
        );
        assert_eq!(
            with_extension("a/notes.md", "zip"),
            PathBuf::from("a/notes.zip")
        );
    }
}
