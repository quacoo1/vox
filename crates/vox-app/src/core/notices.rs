//! Third-party notices (Settings → About → Third-party notices): the licences of what ships inside the binary and asks
//! for its notice to travel with it. Written to `<profile>\third-party-notices.txt` and opened in the default text
//! editor, because the full text (several KB) does not fit a dialog.
//!
//! - Slint: the Slint Royalty-free Desktop, Mobile, and Web Applications License 2.0. Its attribution condition is met
//!   by the `AboutSlint` widget in Settings → About; the licence text is here for reference.
//! - Lucide icons (`ui/icons`): ISC, with the MIT notice of the Feather-derived icons.
//! - The SCOWL-derived word list used by text clean-up (`vox_text::clean::word_list_notice`).
//! - The browser extension's fonts ship their OFL files next to them in `browser-extension\fonts`.

use std::path::{Path, PathBuf};

const SLINT_LICENSE: &str = include_str!("../../assets/licenses/LicenseRef-Slint-Royalty-free-2.0.md");
const LUCIDE_LICENSE: &str = include_str!("../../../../ui/icons/LICENSE-lucide.txt");

/// The file name in the profile folder.
pub const FILE_NAME: &str = "third-party-notices.txt";

/// The whole notices text, one section per component.
pub fn text() -> String {
    let sections = [
        ("Slint 1.18 (user interface toolkit)", SLINT_LICENSE.trim().to_string()),
        ("Lucide icons", LUCIDE_LICENSE.trim().to_string()),
        ("English word list (text clean-up)", vox_text::clean::word_list_notice()),
        (
            "Browser extension fonts",
            "Space Grotesk and Doto are licensed under the SIL Open Font License 1.1. The licence files are in the \
             browser-extension\\fonts folder next to the program."
                .to_string(),
        ),
    ];
    let mut out = String::from("Third-party notices for Vox\n\nVox includes the following third-party software.\n");
    for (title, body) in sections {
        out.push_str(&format!("\n\n{title}\n{}\n\n{body}\n", "=".repeat(title.chars().count())));
    }
    out
}

/// Writes the notices into `folder` (Windows line endings, so Notepad on older Windows shows them right) and returns
/// the file's path.
pub fn write(folder: &Path) -> std::io::Result<PathBuf> {
    let path = folder.join(FILE_NAME);
    let body = text().replace("\r\n", "\n");
    let body = if cfg!(windows) { body.replace('\n', "\r\n") } else { body };
    std::fs::write(&path, body)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_component_is_credited() {
        let t = text();
        for needle in ["Slint Royalty-free Desktop", "AboutSlint", "Lucide Icons and Contributors", "Cole Bemis", "SCOWL", "Kevin Atkinson", "SIL Open Font License"] {
            assert!(t.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn writes_the_file_into_the_folder() {
        let dir = std::env::temp_dir().join(format!("vox-notices-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write(&dir).unwrap();
        assert_eq!(path, dir.join(FILE_NAME));
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("Third-party notices for Vox"));
        assert_eq!(written.contains("\r\n"), cfg!(windows));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
