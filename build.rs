use winres::{VersionInfo, WindowsResource};

fn main() {
    let version = env!("CARGO_PKG_VERSION");

    // Embed the icon and richer PE version metadata into the executable.
    let mut res = WindowsResource::new();
    let numeric_version = pack_version(version);

    res.set_icon("src/icons/icon.ico")
        .set("FileVersion", version)
        .set("ProductVersion", version)
        .set_version_info(VersionInfo::FILEVERSION, numeric_version)
        .set_version_info(VersionInfo::PRODUCTVERSION, numeric_version);

    // Cross-compiling to *-pc-windows-gnu from a non-Windows host: mingw-w64
    // ships the resource compiler / archiver under a target-prefixed name
    // (e.g. x86_64-w64-mingw32-windres) instead of the bare `windres`/`ar`.
    let target = std::env::var("TARGET").unwrap_or_default();
    let host = std::env::var("HOST").unwrap_or_default();
    let cross_gnu = target.ends_with("windows-gnu") && !host.contains("windows");
    if cross_gnu {
        let arch = target.split('-').next().unwrap_or("x86_64");
        let prefix = format!("{arch}-w64-mingw32");
        res.set_windres_path(&format!("{prefix}-windres"));
        res.set_ar_path(&format!("{prefix}-ar"));
    }

    match res.compile() {
        Ok(()) => {
            // winres hands the gnu linker the resource as a static lib, which
            // gets GC'd because no Rust symbol references it. Link the object
            // directly so the icon/version actually end up in the binary.
            if cross_gnu {
                let out_dir = std::env::var("OUT_DIR").unwrap_or_default();
                println!("cargo:rustc-link-arg={out_dir}/resource.o");
            }
        }
        // On a cross-build the resource toolchain may be unavailable; the
        // binary still works without the embedded icon/version metadata.
        Err(error) if cross_gnu => {
            println!("cargo:warning=Skipping Windows resource embedding during cross-build: {error}");
        }
        Err(error) => panic!("Failed to compile Windows resources: {error}"),
    }
}

fn pack_version(version: &str) -> u64 {
    let core = version.split('-').next().unwrap_or(version);
    let mut parts = core.split('.').map(|part| part.parse::<u64>().unwrap_or(0));

    let major = parts.next().unwrap_or(0).min(u16::MAX as u64);
    let minor = parts.next().unwrap_or(0).min(u16::MAX as u64);
    let patch = parts.next().unwrap_or(0).min(u16::MAX as u64);

    (major << 48) | (minor << 32) | (patch << 16)
}
