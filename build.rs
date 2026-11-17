use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=assets/brand/diskweave-icon.ico");
    println!("cargo:rerun-if-changed=resources/app.manifest");
    println!("cargo:rerun-if-env-changed=RC");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let resource = out.join("diskweave.rc");
    let target = out.join("diskweave.res");
    let quote = |p: PathBuf| p.to_string_lossy().replace('\\', "/");
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let numeric = version.replace('.', ",") + ",0";
    fs::write(
        &resource,
        format!(
            r#"#pragma code_page(65001)
1 ICON "{}"
1 24 "{}"
1 VERSIONINFO
 FILEVERSION {numeric}
 PRODUCTVERSION {numeric}
 FILEOS 0x40004
 FILETYPE 1
BEGIN
 BLOCK "StringFileInfo"
 BEGIN
  BLOCK "040904b0"
  BEGIN
   VALUE "CompanyName", "DiskWeave"
   VALUE "FileDescription", "DiskWeave 디스크 용량 분석"
   VALUE "FileVersion", "{version}.0"
   VALUE "ProductName", "DiskWeave"
   VALUE "ProductVersion", "{version}.0"
   VALUE "OriginalFilename", "DiskWeave.exe"
  END
 END
 BLOCK "VarFileInfo"
 BEGIN
  VALUE "Translation", 0x409, 1200
 END
END
"#,
            quote(root.join("assets/brand/diskweave-icon.ico")),
            quote(root.join("resources/app.manifest"))
        ),
    )
    .unwrap();
    let rc = env::var_os("RC").map(PathBuf::from).unwrap_or_else(|| {
        let sdk =
            PathBuf::from(env::var_os("ProgramFiles(x86)").unwrap()).join("Windows Kits/10/bin");
        let mut versions: Vec<_> = fs::read_dir(sdk)
            .expect("Windows SDK Resource Compiler 필요")
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("x64/rc.exe"))
            .filter(|path| path.is_file())
            .collect();
        versions.sort();
        versions.pop().expect("Windows SDK rc.exe 필요")
    });
    let status = Command::new(rc)
        .arg("/nologo")
        .arg(format!("/fo{}", target.display()))
        .arg(&resource)
        .status()
        .expect("Windows 리소스 컴파일 실행 실패");
    assert!(status.success(), "Windows 리소스 컴파일 실패");
    println!("cargo:rustc-link-arg-bin=diskweave={}", target.display());
}
