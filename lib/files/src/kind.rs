//! What a file is, judged by its extension, and the application that opens
//! it by default.
//!
//! [`TYPES`] is the one association table of the system: Files, the
//! Terminal's `open`, the desktop's icons and the pickers of Photos, Music
//! and Settings all ask it, so "which app opens a `.qoa` file" has one
//! answer. Only formats an application can really open are associated with
//! it: Photos opens what `vimage` decodes, Music plays what `vaudio`
//! decodes, and the Text Editor takes text and source files.

use alloc::format;
use alloc::string::{String, ToString};

use crate::path::extension;

/// The broad kind of a file (for icons, colours and filters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// Text and source code.
    Text,
    /// A picture.
    Image,
    /// Sound or music (not every audio format can be played).
    Audio,
    /// A Vindows program (`.exe`).
    Program,
    Other,
}

/// An application that opens files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct App {
    /// Application id (`assets/apps/<id>.app`), for `launcher::launch_app`.
    pub id: &'static str,
    /// The program, for `launcher::launch`.
    pub exe: &'static str,
}

/// The Text Editor.
pub const EDITOR: App = App { id: "editor", exe: "/system/bin/editor.exe" };
/// Photos.
pub const PHOTOS: App = App { id: "photos", exe: "/system/bin/photos.exe" };
/// Music.
pub const MUSIC: App = App { id: "music", exe: "/system/bin/music.exe" };
/// Files, which opens folders.
pub const FILES: App = App { id: "files", exe: "/system/bin/files.exe" };

/// A file type: its extensions (lower case), kind, description and default
/// application.
#[derive(Debug, Clone, Copy)]
pub struct FileType {
    pub extensions: &'static [&'static str],
    pub kind: FileKind,
    pub name: &'static str,
    pub app: Option<App>,
}

const fn text(extensions: &'static [&'static str], name: &'static str) -> FileType {
    FileType { extensions, kind: FileKind::Text, name, app: Some(EDITOR) }
}

const fn image(extensions: &'static [&'static str], name: &'static str) -> FileType {
    FileType { extensions, kind: FileKind::Image, name, app: Some(PHOTOS) }
}

const fn audio(extensions: &'static [&'static str], name: &'static str, app: Option<App>) -> FileType {
    FileType { extensions, kind: FileKind::Audio, name, app }
}

const fn other(extensions: &'static [&'static str], kind: FileKind, name: &'static str) -> FileType {
    FileType { extensions, kind, name, app: None }
}

/// Every known file type.
pub const TYPES: &[FileType] = &[
    text(&["txt"], "Text document"),
    text(&["md"], "Markdown document"),
    text(&["rs"], "Rust source"),
    text(&["c", "h", "cpp", "hpp"], "C/C++ source"),
    text(&["s", "asm"], "Assembly source"),
    text(&["py"], "Python script"),
    text(&["sh"], "Shell script"),
    text(&["cfg", "toml", "ini", "conf", "yaml", "yml"], "Settings file"),
    text(&["json"], "JSON document"),
    text(&["log"], "Log file"),
    text(&["csv"], "CSV table"),
    text(&["html", "css", "js", "xml"], "Web document"),
    text(&["app"], "App manifest"),
    text(&["vts"], "Automation script"),
    image(&["png"], "PNG image"),
    image(&["jpg", "jpeg", "jpe", "jfif"], "JPEG image"),
    image(&["bmp", "dib"], "BMP image"),
    image(&["qoi"], "QOI image"),
    audio(&["wav", "wave"], "WAV audio", Some(MUSIC)),
    audio(&["qoa"], "QOA audio", Some(MUSIC)),
    // Audio formats Vindows cannot play yet.
    audio(&["mp3"], "MP3 audio", None),
    audio(&["ogg", "opus"], "Ogg audio", None),
    audio(&["flac"], "FLAC audio", None),
    audio(&["mid", "midi"], "MIDI music", None),
    audio(&["mod", "xm", "s3m", "it"], "Tracker music", None),
    audio(&["aac", "m4a"], "AAC audio", None),
    other(&["exe"], FileKind::Program, "Program"),
    other(&["ttf", "otf"], FileKind::Other, "Font"),
    other(&["img", "iso"], FileKind::Other, "Disk image"),
];

/// The type of the file `name` (a name or a path), if it is known.
pub fn file_type(name: &str) -> Option<&'static FileType> {
    let ext = extension(name);
    if ext.is_empty() {
        return None;
    }
    TYPES.iter().find(|t| t.extensions.contains(&ext.as_str()))
}

/// The kind of the file `name`.
pub fn file_kind(name: &str) -> FileKind {
    file_type(name).map_or(FileKind::Other, |t| t.kind)
}

/// The application that opens the file `name` by default, if any. (Folders
/// open in [`FILES`].)
pub fn default_app(name: &str) -> Option<App> {
    file_type(name).and_then(|t| t.app)
}

/// True for pictures Vindows can decode — the files Photos opens.
pub fn is_image(name: &str) -> bool {
    default_app(name) == Some(PHOTOS)
}

/// True for sound files Vindows can play — the files Music opens.
pub fn is_playable_audio(name: &str) -> bool {
    default_app(name) == Some(MUSIC)
}

/// A short description of a file's type: "PNG image", "Folder", or for an
/// unknown extension "XYZ file".
pub fn kind_name(name: &str, is_dir: bool) -> String {
    if is_dir {
        return "Folder".to_string();
    }
    if let Some(t) = file_type(name) {
        return t.name.to_string();
    }
    let ext = extension(name);
    if ext.is_empty() { "File".to_string() } else { format!("{} file", ext.to_ascii_uppercase()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_by_extension() {
        assert_eq!(file_kind("notes.TXT"), FileKind::Text);
        assert_eq!(file_kind("/home/user/primes.rs"), FileKind::Text);
        assert_eq!(file_kind("Photo.jpeg"), FileKind::Image);
        assert_eq!(file_kind("song.qoa"), FileKind::Audio);
        assert_eq!(file_kind("song.mp3"), FileKind::Audio);
        assert_eq!(file_kind("init.exe"), FileKind::Program);
        assert_eq!(file_kind("Inter.otf"), FileKind::Other);
        assert_eq!(file_kind("Makefile"), FileKind::Other);
        assert_eq!(file_kind(".png"), FileKind::Other);
    }

    #[test]
    fn default_apps() {
        assert_eq!(default_app("a.md"), Some(EDITOR));
        assert_eq!(default_app("settings.app"), Some(EDITOR));
        assert_eq!(default_app("Lighthouse.JPG"), Some(PHOTOS));
        assert_eq!(default_app("x.jfif"), Some(PHOTOS));
        assert_eq!(default_app("Retro Sunset.bmp"), Some(PHOTOS));
        assert_eq!(default_app("Calibration.qoa"), Some(MUSIC));
        assert_eq!(default_app("take.wave"), Some(MUSIC));
        // Music cannot play these, so nothing opens them by default.
        assert_eq!(default_app("song.mp3"), None);
        assert_eq!(default_app("tune.mid"), None);
        assert_eq!(default_app("init.exe"), None);
        assert_eq!(default_app("archive.zip"), None);
        assert_eq!(default_app("README"), None);
        assert_eq!(PHOTOS.exe, "/system/bin/photos.exe");
        assert!(is_image("a.qoi") && !is_image("a.mp3") && !is_image("png"));
        assert!(is_playable_audio("A.WAV") && !is_playable_audio("a.flac"));
    }

    #[test]
    fn descriptions() {
        assert_eq!(kind_name("anything", true), "Folder");
        assert_eq!(kind_name("a.png", false), "PNG image");
        assert_eq!(kind_name("a.JPG", false), "JPEG image");
        assert_eq!(kind_name("a.zip", false), "ZIP file");
        assert_eq!(kind_name("LICENSE", false), "File");
    }

    #[test]
    fn every_extension_appears_once() {
        for (i, t) in TYPES.iter().enumerate() {
            for e in t.extensions {
                assert_eq!(*e, e.to_ascii_lowercase(), "extensions are lower case");
                for u in &TYPES[i + 1..] {
                    assert!(!u.extensions.contains(e), "'{e}' is listed twice");
                }
            }
        }
    }
}
