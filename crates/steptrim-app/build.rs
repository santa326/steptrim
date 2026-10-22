//! Embeds the Windows icon and version information into steptrim.exe.
fn main() {
    println!("cargo:rerun-if-changed=assets/steptrim.ico");
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/steptrim.ico");
        res.set("ProductName", "StepTrim");
        res.set("FileDescription", "StepTrim: trim large STEP assemblies");
        res.set("LegalCopyright", "Copyright (c) 2026 Ravi Teja Potineni. MIT License.");
        res.compile().expect("embed Windows resources");
    }
}
