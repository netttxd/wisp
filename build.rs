// Puts version info (name, maker, version) into wisp.exe, like every normal Windows app has. Without it antivirus
// trusts the exe less, and Task Manager shows no name. Needs windres (comes with MinGW); without it the exe is
// built without version info.
use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let v = env::var("CARGO_PKG_VERSION").unwrap();
    let n: Vec<u32> = v.split('.').map(|p| p.parse().unwrap_or(0)).chain([0; 4]).take(4).collect();
    let nums = format!("{},{},{},{}", n[0], n[1], n[2], n[3]);
    let rc = format!(
        r#"1 VERSIONINFO
FILEVERSION {nums}
PRODUCTVERSION {nums}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Wisp"
      VALUE "FileDescription", "Wisp"
      VALUE "FileVersion", "{v}"
      VALUE "InternalName", "wisp"
      VALUE "LegalCopyright", "Wisp contributors"
      VALUE "OriginalFilename", "wisp.exe"
      VALUE "ProductName", "Wisp"
      VALUE "ProductVersion", "{v}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let (src, obj) = (out.join("version.rc"), out.join("version.o"));
    fs::write(&src, rc).unwrap();
    let made = Command::new("windres").arg(&src).args(["-O", "coff", "-o"]).arg(&obj).status().is_ok_and(|s| s.success());
    if made {
        println!("cargo:rustc-link-arg-bins={}", obj.display());
    } else {
        println!("cargo:warning=windres not found: wisp.exe is built without version info");
    }
}
