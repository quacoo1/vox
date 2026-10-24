//! 1. Compiles the whole UI (ui/app.slint and everything it imports) with the Fluent style. The generated Rust lives
//!    in `src/ui/generated.rs` via `slint::include_modules!()`.
//! 2. Stamps the build hash for Settings → About (BRIEF.md "About shows the app version and build hash"):
//!    `VOX_BUILD_HASH` from the environment if set (CI), else `git rev-parse --short=10 HEAD`, else `dev`. Exposed as
//!    `env!("VOX_BUILD_HASH")` (read by `runtime::Runtime::detect`).
//! 3. (P-APP-WIN, D-SH-20) Embeds the Windows resources so Explorer, the taskbar, Alt+Tab, Task Manager and the
//!    Startup-apps list show the Vox name and icon: ICON group id 1 = `assets/vox.ico` (16/20/24/32/40/48/64/256 px,
//!    the DESIGN.md "V" wave mark on its dark tile, drawn by `tray::mark` and checked in by the `#[ignore]` test
//!    `tray::tests::generate_app_icon_assets`), a VERSIONINFO (`FileDescription` and `ProductName` `Vox`,
//!    `FileVersion`/`ProductVersion` = `CARGO_PKG_VERSION`, `OriginalFilename` `vox.exe`, `CompanyName` `Vox`,
//!    `LegalCopyright` `Copyright © Vox`) and an application manifest (PerMonitorV2 `dpiAwareness`, Common-Controls v6,
//!    Windows 10/11 `supportedOS`, `asInvoker`). The workspace has no resource-compiler dependency (`winresource`
//!    would need `rc.exe` from a Windows SDK on every machine), so this script writes the compiled `.res` file itself
//!    (the documented 32-bit resource file format: `RT_ICON` ×n + `RT_GROUP_ICON` + `RT_VERSION` + `RT_MANIFEST`) and
//!    hands it to the MSVC linker, which accepts `.res` inputs directly (`/MANIFEST:NO` keeps the linker from adding a
//!    second manifest). The Explorer verb's `Icon` value `"<exe>",0` then resolves (D-INT-7). `assets/vox-256.png` is
//!    the AUMID `IconUri` source (`app::run`); the main window's icons are drawn at run time from the same mark
//!    (`ui::set_window_icon`: 48 px small icon, 256 px big icon = the pixels of `assets/vox-256.png`).
//! 4. (P-APP-INT, D-INT-13, R-Installer) Mirrors the repository's `browser-extension\` (shipped unchanged) next to the
//!    built `vox.exe` (`target\<profile>\browser-extension\`), so the portable layout `vox.exe` + `browser-extension\`
//!    is what every build produces and `runtime::extension_dir` finds the folder beside the exe. Files are written only
//!    when their bytes differ; files that no longer exist in the source are removed from the copy. A failure is a
//!    build warning, never an error (the dev fallback is the repository folder).

use std::path::{Path, PathBuf};

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("../../ui/app.slint", config).expect("compiling ui/app.slint failed");
    println!("cargo:rerun-if-changed=../../ui");

    let hash = std::env::var("VOX_BUILD_HASH").ok().filter(|h| !h.trim().is_empty()).or_else(git_hash).unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=VOX_BUILD_HASH={hash}");
    println!("cargo:rerun-if-env-changed=VOX_BUILD_HASH");

    embed_windows_resources();
    copy_browser_extension();
}

// ───────────────────────────── browser extension next to the exe ─────────────────────────────

fn copy_browser_extension() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()));
    let source = manifest_dir.join("..").join("..").join("browser-extension");
    println!("cargo:rerun-if-changed={}", source.display());
    // OUT_DIR = <target>\[<triple>\]<profile>\build\vox-app-<hash>\out: three levels up is the folder of vox.exe.
    let Some(exe_dir) = std::env::var_os("OUT_DIR").map(PathBuf::from).and_then(|out| out.ancestors().nth(3).map(Path::to_path_buf)) else {
        return;
    };
    if !source.is_dir() {
        println!("cargo:warning=browser-extension\\ is missing: the build has no extension folder next to vox.exe");
        return;
    }
    if let Err(e) = mirror_dir(&source, &exe_dir.join("browser-extension")) {
        println!("cargo:warning=could not copy browser-extension next to vox.exe: {e}");
    }
}

/// Makes `dest` an exact copy of `source` (recursively), writing only files whose bytes differ.
fn mirror_dir(source: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    let mut names = std::collections::HashSet::new();
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let (from, to) = (entry.path(), dest.join(&name));
        names.insert(name);
        if entry.file_type()?.is_dir() {
            mirror_dir(&from, &to)?;
        } else {
            let bytes = std::fs::read(&from)?;
            if std::fs::read(&to).ok().as_deref() != Some(bytes.as_slice()) {
                std::fs::write(&to, bytes)?;
            }
        }
    }
    for entry in std::fs::read_dir(dest)? {
        let entry = entry?;
        if !names.contains(&entry.file_name()) {
            if entry.file_type()?.is_dir() {
                std::fs::remove_dir_all(entry.path())?;
            } else {
                std::fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}

fn git_hash() -> Option<String> {
    let out = std::process::Command::new("git").args(["rev-parse", "--short=10", "HEAD"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let h = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!h.is_empty()).then_some(h)
}

// ───────────────────────────── Windows resources ─────────────────────────────

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
const RT_MANIFEST: u16 = 24;
/// `MOVEABLE | PURE | DISCARDABLE` (ignored by Win32, written like rc.exe does).
const MEMORY_FLAGS: u16 = 0x1030;
/// en-US.
const LANGUAGE: u16 = 0x0409;

fn embed_windows_resources() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" || env != "msvc" {
        return;
    }
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()));
    let ico_path = manifest_dir.join("assets").join("vox.ico");
    println!("cargo:rerun-if-changed={}", ico_path.display());
    let ico = match std::fs::read(&ico_path) {
        Ok(bytes) => Some(bytes),
        Err(_) => {
            println!("cargo:warning=assets/vox.ico is missing: vox.exe is built without an icon");
            None
        }
    };
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let res = build_res(ico.as_deref(), &version);
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR")).join("vox.res");
    if let Err(e) = std::fs::write(&out, res) {
        println!("cargo:warning=could not write {}: {e}", out.display());
        return;
    }
    println!("cargo:rustc-link-arg-bins={}", link_path(&out));
    println!("cargo:rustc-link-arg-bins=/MANIFEST:NO");
}

fn link_path(p: &Path) -> String {
    p.display().to_string()
}

/// The whole `.res` file.
fn build_res(ico: Option<&[u8]>, version: &str) -> Vec<u8> {
    let mut res = Vec::new();
    // The file starts with an empty entry (type 0, name 0) that marks it as a 32-bit resource file.
    write_entry(&mut res, 0, 0, &[], 0, 0);
    if let Some(ico) = ico {
        match parse_ico(ico) {
            Some(images) => {
                let mut group = Vec::new();
                group.extend_from_slice(&0u16.to_le_bytes());
                group.extend_from_slice(&1u16.to_le_bytes());
                group.extend_from_slice(&(images.len() as u16).to_le_bytes());
                for (i, img) in images.iter().enumerate() {
                    let id = (i + 1) as u16;
                    write_entry(&mut res, RT_ICON, id, img.data, MEMORY_FLAGS, LANGUAGE);
                    group.extend_from_slice(&img.dir[..12]);
                    group.extend_from_slice(&id.to_le_bytes());
                }
                write_entry(&mut res, RT_GROUP_ICON, 1, &group, MEMORY_FLAGS, LANGUAGE);
            }
            None => println!("cargo:warning=assets/vox.ico is not a valid icon file: vox.exe is built without an icon"),
        }
    }
    write_entry(&mut res, RT_VERSION, 1, &version_info(version), MEMORY_FLAGS, LANGUAGE);
    write_entry(&mut res, RT_MANIFEST, 1, manifest(version).as_bytes(), MEMORY_FLAGS, LANGUAGE);
    res
}

/// One resource: header (ordinal type and name) + data, DWORD-aligned.
fn write_entry(out: &mut Vec<u8>, ty: u16, name: u16, data: &[u8], flags: u16, lang: u16) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes());
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&name.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&lang.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // Version
    out.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    out.extend_from_slice(data);
    pad4(out);
}

fn pad4(b: &mut Vec<u8>) {
    while !b.len().is_multiple_of(4) {
        b.push(0);
    }
}

/// One image of an `.ico`: its 16-byte directory entry and its data.
struct IcoImage<'a> {
    dir: &'a [u8],
    data: &'a [u8],
}

fn parse_ico(ico: &[u8]) -> Option<Vec<IcoImage<'_>>> {
    let u16_at = |o: usize| ico.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |o: usize| ico.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if u16_at(0)? != 0 || u16_at(2)? != 1 {
        return None;
    }
    let count = u16_at(4)? as usize;
    if count == 0 {
        return None;
    }
    let mut images = Vec::with_capacity(count);
    for i in 0..count {
        let e = 6 + 16 * i;
        let dir = ico.get(e..e + 16)?;
        let len = u32_at(e + 8)? as usize;
        let off = u32_at(e + 12)? as usize;
        let data = ico.get(off..off.checked_add(len)?)?;
        images.push(IcoImage { dir, data });
    }
    Some(images)
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain(std::iter::once(0)).flat_map(|u| u.to_le_bytes()).collect()
}

/// A `VS_VERSIONINFO`-style node: `wLength, wValueLength, wType, szKey, Padding1, Value, Padding2, Children`
/// (`wLength` excludes the node's own trailing padding; children are DWORD-aligned).
fn node(key: &str, value: &[u8], value_length: u16, text: bool, children: &[Vec<u8>]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&value_length.to_le_bytes());
    b.extend_from_slice(&(text as u16).to_le_bytes());
    b.extend_from_slice(&utf16z(key));
    pad4(&mut b);
    b.extend_from_slice(value);
    for child in children {
        pad4(&mut b);
        b.extend_from_slice(child);
    }
    let len = b.len() as u16;
    b[0..2].copy_from_slice(&len.to_le_bytes());
    b
}

fn string_node(key: &str, value: &str) -> Vec<u8> {
    let v = utf16z(value);
    node(key, &v, (v.len() / 2) as u16, true, &[])
}

/// `major.minor.patch` → `(MS, LS)` of a `VS_FIXEDFILEINFO` version.
fn version_words(version: &str) -> (u32, u32) {
    let mut parts = version.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse::<u32>().unwrap_or(0).min(0xFFFF));
    let (a, b, c) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    ((a << 16) | b, c << 16)
}

fn version_info(version: &str) -> Vec<u8> {
    let (ms, ls) = version_words(version);
    let mut fixed = Vec::with_capacity(52);
    for v in [
        0xFEEF_04BDu32, // dwSignature
        0x0001_0000,    // dwStrucVersion
        ms,
        ls, // file version
        ms,
        ls,          // product version
        0x3F,        // dwFileFlagsMask
        0,           // dwFileFlags
        0x0004_0004, // VOS_NT_WINDOWS32
        1,           // VFT_APP
        0,           // dwFileSubtype
        0,
        0, // file date
    ] {
        fixed.extend_from_slice(&v.to_le_bytes());
    }
    let strings = [
        string_node("CompanyName", "Vox"),
        string_node("FileDescription", "Vox"),
        string_node("FileVersion", version),
        string_node("InternalName", "vox"),
        string_node("LegalCopyright", "Copyright \u{00A9} Vox"),
        string_node("OriginalFilename", "vox.exe"),
        string_node("ProductName", "Vox"),
        string_node("ProductVersion", version),
    ];
    let table = node("040904B0", &[], 0, true, &strings);
    let string_file_info = node("StringFileInfo", &[], 0, true, &[table]);
    let translation: Vec<u8> = [0x0409u16, 0x04B0].iter().flat_map(|w| w.to_le_bytes()).collect();
    let var = node("Translation", &translation, 4, false, &[]);
    let var_file_info = node("VarFileInfo", &[], 0, true, &[var]);
    node("VS_VERSION_INFO", &fixed, 52, false, &[string_file_info, var_file_info])
}

fn manifest(version: &str) -> String {
    let (ms, ls) = version_words(version);
    let v4 = format!("{}.{}.{}.0", ms >> 16, ms & 0xFFFF, ls >> 16);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="Vox.Reader" version="{v4}" processorArchitecture="*"/>
  <description>Vox</description>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>
"#
    )
}
