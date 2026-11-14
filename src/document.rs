use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::editor::Editor;
use crate::image_view::{is_image, ImageView};
use crate::unreal::UnrealProfile;
use crate::syntax::{self, Language};

/// A file open in a tab: source text, or an image.
///
/// Text is kept with `\n` line endings in memory; the original line ending
/// is restored on save.
pub struct Document {
    pub path: PathBuf,
    /// Detected language, or `None` for plain text.
    pub language: Option<&'static Language>,
    /// Text, highlighting, selection and undo history. Empty for images.
    pub editor: Editor,
    /// Set for image files, which are shown instead of the (empty) text.
    pub image: Option<ImageView>,
    crlf: bool,
}

impl Document {
    /// Open `path`. With an Unreal profile, C++ highlighting understands the
    /// engine's reflection macros.
    pub fn open(path: &Path, unreal: Option<Arc<UnrealProfile>>) -> std::io::Result<Self> {
        if is_image(path) {
            return Ok(Self {
                path: path.to_path_buf(),
                language: None,
                editor: Editor::new(String::new(), None),
                image: Some(ImageView::load(path)?),
                crlf: false,
            });
        }
        let bytes = std::fs::read(path)?;
        // Source files are not always valid UTF-8; show them anyway.
        let text = String::from_utf8_lossy(&bytes);
        let crlf = text.contains("\r\n");
        let text = if crlf { text.replace("\r\n", "\n") } else { text.into_owned() };

        let language = syntax::detect(path);
        let editor = Editor::with_unreal(text, language, unreal);
        Ok(Self { path: path.to_path_buf(), language, editor, image: None, crlf })
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        if self.image.is_some() {
            return Ok(()); // images are view-only
        }
        let text = self.editor.text();
        if self.crlf {
            std::fs::write(&self.path, text.replace('\n', "\r\n"))?;
        } else {
            std::fs::write(&self.path, text)?;
        }
        self.editor.mark_saved();
        Ok(())
    }

    /// True when there are changes that have not been saved.
    pub fn is_dirty(&self) -> bool {
        self.editor.is_dirty()
    }

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    pub fn language_name(&self) -> &'static str {
        if self.image.is_some() {
            return "PNG Image";
        }
        self.language.map_or("Plain Text", |l| l.name)
    }
}

/// A key for deciding whether two paths name the same file.
pub fn path_key(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    // Windows paths are case-insensitive.
    if cfg!(windows) {
        PathBuf::from(absolute.to_string_lossy().to_lowercase())
    } else {
        absolute
    }
}
