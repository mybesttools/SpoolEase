// use std::env;
// use std::path::Path;
// use std::process::Command;

use walkdir::WalkDir;

fn main() {
    generate_translations();
    // // Run wasm-pack build in the second project directory
    //
    // let device_wasm_project_path = std::path::Path::new("../device-wasm");
    //
    // let mut cmd = std::process::Command::new("wasm-pack");
    // let cmd = cmd
    //     .arg("build")
    //     .arg("--release")
    //     .arg("--target")
    //     .arg("web") // or another target like "nodejs"
    //     .current_dir(device_wasm_project_path);
    //
    // for (key, _) in std::env::vars() {
    //     if key.starts_with("CARGO") {
    //         cmd.env_remove(&key);
    //     }
    // }
    //
    // cmd.env_remove("RUSTFLAGS");
    // cmd.env_remove("RUSTUP_TOOLCHAIN"); // Ensures rustup picks the correct one
    // cmd.env_remove("RUSTC");
    //
    // let status = cmd.status().expect("Failed to execute wasm-pack");
    //
    // if !status.success() {
    //     panic!("wasm-pack build failed");
    // }
    //
    // // Now, the second project is built, and you can include the output
    // let output_dir = device_wasm_project_path.join("pkg");
    // let output_file = output_dir.join("device_wasm_bg.wasm");
    //
    // // Make sure the file exists
    // if !output_file.exists() {
    //     panic!("Output file not found after wasm-pack build");
    // }
    //
    // // println!("cargo:rerun-if-changed={}", output_file.display());
    // println!("cargo:rerun-if-changed={}", device_wasm_project_path.display());

    // Slint needs to come last, seems like it syncs in some way with the build and waits to the end

    for entry in WalkDir::new("static").into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            println!("cargo:rerun-if-changed={}", entry.path().display());
        }
    }
    for entry in WalkDir::new("../inventory/dist").into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            println!("cargo:rerun-if-changed={}", entry.path().display());
        }
    }

    slint_build::compile_with_config(
        "ui/appwindow.slint",
        slint_build::CompilerConfiguration::new().embed_resources(slint_build::EmbedResourcesKind::EmbedForSoftwareRenderer),
    )
    .unwrap();
}

fn generate_translations() {
    use std::collections::BTreeMap;
    use serde_json::Value;

    // Scan translations/ directory at compile time so adding a new *.json
    // is enough to make the language available — no code changes needed.
    println!("cargo:rerun-if-changed=translations");
    println!("cargo:rerun-if-changed=translations/en.json");

    let mut languages: Vec<(String, String)> = Vec::new(); // (code, display_name)
    let mut entries: Vec<_> = std::fs::read_dir("translations")
        .expect("Cannot read translations/")
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let fname = entry.file_name().to_string_lossy().to_string();
        if fname == "en.json" || !fname.ends_with(".json") { continue; }
        let code = fname.trim_end_matches(".json").to_string();
        println!("cargo:rerun-if-changed=translations/{fname}");
        let lang_str = std::fs::read_to_string(format!("translations/{fname}")).unwrap_or_default();
        let lang_data: BTreeMap<String, String> = serde_json::from_str(&lang_str).unwrap_or_default();
        let display_name = lang_data.get("_name").cloned().unwrap_or_else(|| code.clone());
        languages.push((code, display_name));
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let out_path = std::path::PathBuf::from(&out_dir).join("translations_generated.rs");

    let schema_str = std::fs::read_to_string("translations/en.json")
        .expect("Cannot read translations/en.json");
    let schema: BTreeMap<String, Value> = serde_json::from_str(&schema_str)
        .expect("Invalid JSON in translations/en.json");

    let mut output = String::new();

    output.push_str("use slint::ComponentHandle;\n\n");

    // === fn apply_slint_translations ===
    output.push_str("pub fn apply_slint_translations(\n");
    output.push_str("    ui_weak: &slint::Weak<crate::app::AppWindow>,\n");
    output.push_str("    language: &str,\n");
    output.push_str(") {\n");
    output.push_str("    let ui = ui_weak.unwrap();\n");
    output.push_str("    let tr = ui.global::<crate::app::Translations>();\n");
    output.push_str("    match language {\n");

    for (code, _) in &languages {
        let lang_str = std::fs::read_to_string(format!("translations/{code}.json"))
            .unwrap_or_default();
        let lang_data: BTreeMap<String, String> = serde_json::from_str(&lang_str)
            .unwrap_or_default();

        output.push_str(&format!("        \"{code}\" => {{\n"));

        for (key, def) in &schema {
            let slint_prop = match def.get("slint").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => continue,
            };
            let en_value = def.get("en").and_then(|v| v.as_str()).unwrap_or("");
            let value = lang_data.get(key.as_str()).map(|s| s.as_str()).unwrap_or(en_value);

            let escaped = value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r");

            output.push_str(&format!("            tr.set_tr_{slint_prop}(\"{escaped}\".into());\n"));
        }

        output.push_str("        }\n");
    }

    // Explicit English arm: reset all properties to their English defaults
    output.push_str("        \"en\" | _ => {\n");
    for (key, def) in &schema {
        let slint_prop = match def.get("slint").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => continue,
        };
        let en_value = def.get("en").and_then(|v| v.as_str()).unwrap_or("");
        let escaped = en_value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r");
        let _ = key; // suppress unused warning
        output.push_str(&format!("            tr.set_tr_{slint_prop}(\"{escaped}\".into());\n"));
    }
    output.push_str("        }\n");

    output.push_str("    }\n");
    output.push_str("}\n\n");

    // === fn get_web_translations_json ===
    output.push_str("pub fn get_web_translations_json(lang: &str) -> &'static str {\n");
    output.push_str("    match lang {\n");

    for (lang, _display_name) in &languages {
        let lang_str = std::fs::read_to_string(format!("translations/{lang}.json"))
            .unwrap_or_default();
        let lang_data: BTreeMap<String, String> = serde_json::from_str(&lang_str)
            .unwrap_or_default();

        let mut web_table: BTreeMap<String, String> = BTreeMap::new();
        for (key, def) in &schema {
            let web_key = match def.get("web").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => continue,
            };
            let en_value = def.get("en").and_then(|v| v.as_str()).unwrap_or("");
            let value = lang_data.get(key.as_str()).cloned()
                .unwrap_or_else(|| en_value.to_string());
            web_table.insert(web_key.to_string(), value);
        }

        let web_json = serde_json::to_string(&web_table)
            .expect("Cannot serialize web translations");

        // r##"..."## avoids needing to escape the JSON double-quotes
        output.push_str(&format!("        \"{lang}\" => r##\"{web_json}\"##,\n"));
    }

    output.push_str("        _ => \"{}\",\n");
    output.push_str("    }\n");
    output.push_str("}\n\n");

    // === fn get_available_languages ===
    output.push_str("pub fn get_available_languages() -> &'static [(&'static str, &'static str)] {\n");
    output.push_str("    &[\n");
    output.push_str("        (\"en\", \"English\"),\n");
    for (lang, name) in &languages {
        output.push_str(&format!("        (\"{lang}\", \"{name}\"),\n"));
    }
    output.push_str("    ]\n");
    output.push_str("}\n");

    // === fn tr_rust ===
    // Provides runtime translation for Rust-side strings (OTA status etc.)
    output.push_str("pub fn tr_rust<'a>(lang: &str, key: &'a str) -> &'a str {\n");
    output.push_str("    match (lang, key) {\n");

    // Collect rust-tagged keys
    let rust_keys: Vec<(&String, &serde_json::Value)> = schema.iter()
        .filter(|(_, def)| def.get("rust").and_then(|v| v.as_bool()).unwrap_or(false))
        .collect();

    // Per-language arms for each rust key
    for (code, _) in &languages {
        let lang_str = std::fs::read_to_string(format!("translations/{code}.json"))
            .unwrap_or_default();
        let lang_data: BTreeMap<String, String> = serde_json::from_str(&lang_str)
            .unwrap_or_default();
        for (key, def) in &rust_keys {
            let en_value = def.get("en").and_then(|v| v.as_str()).unwrap_or("");
            let value = lang_data.get(key.as_str()).map(|s| s.as_str()).unwrap_or(en_value);
            let escaped = value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r");
            output.push_str(&format!("        (\"{code}\", \"{key}\") => \"{escaped}\",\n"));
        }
    }

    // English / fallback arms for each rust key
    for (key, def) in &rust_keys {
        let en_value = def.get("en").and_then(|v| v.as_str()).unwrap_or("");
        let escaped = en_value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r");
        output.push_str(&format!("        (_, \"{key}\") => \"{escaped}\",\n"));
    }

    output.push_str("        (_, _) => key,\n");
    output.push_str("    }\n");
    output.push_str("}\n");

    std::fs::write(&out_path, &output).expect("Cannot write translations_generated.rs");
}
