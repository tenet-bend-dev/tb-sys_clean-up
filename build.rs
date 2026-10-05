use std::path::Path;

#[cfg(windows)]
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    let mut res = winresource::WindowsResource::new();

    if Path::new("assets/icon.ico").exists() {
        res.set_icon("assets/icon.ico");
    } else {
        println!("cargo:warning=assets/icon.ico not found - building without an app icon");
    }

    res.set("ProductName", "TB ⧉ Sys_Clean-up");
    res.set("FileDescription", "Windows system maintenance and clean-up utility");
    res.set("LegalCopyright", "All rights reserved");
    res.set_manifest(
        r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </windowsSettings>
  </application>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>"#,
    );

    if let Err(e) = res.compile() {
        println!("cargo:warning=failed to embed Windows resources: {e}");
    }
}

#[cfg(not(windows))]
fn main() {
    let _ = Path::new("assets/icon.ico");
}