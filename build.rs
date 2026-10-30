//! Windows SDK resource compiler only; no build dependencies or runtime service.
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};
fn main() {
    println!("cargo:rerun-if-changed=assets/icons/congminal.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let icon = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("assets/icons/congminal.ico");
    let rc = output.join("congminal-icon.rc");
    let res = output.join("congminal-icon.res");
    fs::write(&rc, format!("1 ICON \"{}\"\n", icon.to_string_lossy().replace('\\', "/"))).unwrap();
    let compiler = find_rc().expect("Windows SDK rc.exe is required to embed the application icon");
    let status = Command::new(compiler)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res)
        .arg(&rc)
        .status()
        .expect("run Windows resource compiler");
    assert!(status.success(), "compile Windows icon resources");
    println!("cargo:rustc-link-arg-bin=congminal={}", res.display());
}
fn find_rc() -> Option<PathBuf> {
    if Command::new("rc.exe").arg("/?").output().is_ok() {
        return Some("rc.exe".into());
    }
    let root = env::var_os("ProgramFiles(x86)").or_else(|| env::var_os("ProgramFiles"))?;
    let base = Path::new(&root).join("Windows Kits/10/bin");
    let mut versions = fs::read_dir(base)
        .ok()?
        .take(64)
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let value = name.to_str()?;
            let parts = value.split('.').map(str::parse::<u32>).collect::<Result<Vec<_>, _>>().ok()?;
            if parts.len() != 4 {
                return None;
            }
            let compiler = entry.path().join("x64/rc.exe");
            compiler.is_file().then_some((parts, compiler))
        })
        .collect::<Vec<_>>();
    versions.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    versions.pop().map(|v| v.1)
}
