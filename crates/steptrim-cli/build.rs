//! Embeds the icon and version information into the command-line tools so
//! every StepTrim binary carries the same product name and version (a
//! requirement of SignPath Foundation's metadata restrictions).
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../steptrim-app/assets/steptrim.ico");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../steptrim-app/assets/steptrim.ico");
        res.set("ProductName", "StepTrim");
        res.set("FileDescription", "StepTrim command-line tool");
        res.set("LegalCopyright", "Copyright (c) 2026 Ravi Teja Potineni. MIT License.");
        res.compile().expect("embed Windows resources");
    }
}
