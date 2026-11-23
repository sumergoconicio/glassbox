// Module: glassbox-cli build support.
// File: build.rs.
// Responsibility: Generate the upstream CLI adapter from unchanged source and manifest anchors.
use std::{env, fs, path::PathBuf};

/// Replace one required source anchor and reject incompatible upstream drift.
///
/// Accepts source as the generated Rust text, needle as one exact anchor, and replacement as its substitute.
/// Returns the transformed source String.
/// Panics with the anchor and observed count when the anchor is absent or ambiguous.
fn replace_once(source: &str, needle: &str, replacement: &str) -> String {
    let count = source.matches(needle).count();
    assert_eq!(
        count, 1,
        "Glassbox upstream adapter anchor count changed for {needle:?}: {count}; review the adapter before building"
    );
    source.replacen(needle, replacement, 1)
}

/// Resolve the upstream checkout used by the generated adapter.
///
/// Accepts no parameters and reads GLASSBOX_UPSTREAM_ROOT or the downstream manifest location.
/// Returns the upstream repository root PathBuf.
/// Panics when CARGO_MANIFEST_DIR is unavailable or the configured root has no liteparse manifest.
fn upstream_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = env::var_os("GLASSBOX_UPSTREAM_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("../../.."));
    assert!(
        root.join("crates/liteparse/Cargo.toml").is_file(),
        "Glassbox upstream root does not contain crates/liteparse/Cargo.toml: {}",
        root.display()
    );
    root
}

/// Read the upstream package version without maintaining a downstream copy.
///
/// Accepts manifest as the unchanged upstream Cargo.toml text.
/// Returns the liteparse package version as an owned String.
/// Panics when the package/version anchors are missing, ambiguous, or malformed.
fn upstream_version(manifest: &str) -> String {
    let package = replace_once(
        manifest,
        "[package]\nname = \"liteparse\"\n",
        "[package]\nname = \"liteparse\"\n",
    );
    let after_name = package
        .split("name = \"liteparse\"\n")
        .nth(1)
        .expect("validated liteparse package name");
    let version_line = after_name
        .lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .expect("upstream liteparse package version anchor missing");
    let version = version_line
        .strip_suffix('"')
        .expect("upstream liteparse package version must be a quoted scalar");
    assert!(
        !version.is_empty()
            && version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-+".contains(&byte)),
        "unsupported upstream package version for generated Rust environment: {version:?}"
    );
    version.to_string()
}

/// Generate the async Glassbox adapter and the unbranded upstream inventory probe.
///
/// Accepts source as unchanged upstream main.rs text.
/// Returns the Glassbox adapter and upstream inventory source strings.
/// Panics when any required Clap, command identity, entrypoint, or adapter-symbol anchor drifts.
fn generate_sources(source: &str) -> (String, String) {
    assert!(
        !source.contains("fn glassbox_args()") && !source.contains("fn glassbox_probe_inventory()"),
        "upstream source already contains a Glassbox adapter symbol"
    );
    let versioned = replace_once(
        source,
        "    version,\n    about = \"OSS document parsing tool",
        "    version = env!(\"GLASSBOX_UPSTREAM_VERSION\"),\n    about = \"OSS document parsing tool",
    );
    let clap_import = replace_once(
        &versioned,
        "use clap::{Args, Parser, Subcommand};",
        "use clap::{Args, CommandFactory, Parser, Subcommand};",
    );
    let glassbox_named = replace_once(&clap_import, "name = \"lit\",", "name = \"glassbox\",");
    let glassbox_main = replace_once(
        &glassbox_named,
        "#[tokio::main]\nasync fn main() -> Result<(), Box<dyn std::error::Error>> {\n    let cli = Cli::parse();",
        "/// Execute the unchanged upstream command implementation through Glassbox routing.\n///\n/// Accepts no parameters and reads the process argument vector as OsStrings.\n/// Returns the upstream Result after asynchronous processing completes.\n/// Returns upstream parsing, conversion, rendering, output, and runtime errors unchanged.\npub async fn run() -> Result<(), Box<dyn std::error::Error>> {\n    glassbox_probe_inventory();\n    let cli = Cli::parse_from(glassbox_args());",
    );
    let upstream_main = replace_once(
        &clap_import,
        "    let cli = Cli::parse();",
        "    glassbox_probe_inventory();\n    let cli = Cli::parse();",
    );
    let glassbox = replace_once(
        &glassbox_main,
        "#[cfg(test)]\nmod tests {",
        &format!("{ROUTER_SUPPORT}{INVENTORY_SUPPORT}\n#[cfg(test)]\nmod tests {{"),
    );
    let upstream_inventory = replace_once(
        &upstream_main,
        "#[cfg(test)]\nmod tests {",
        &format!("{INVENTORY_SUPPORT}\n#[cfg(test)]\nmod tests {{"),
    );
    (glassbox, upstream_inventory)
}

/// Build both generated sources and register every adapter input for rebuilding.
///
/// Accepts no parameters and reads the upstream root, main.rs, manifest, and OUT_DIR environment.
/// Returns unit after writing generated Rust beneath Cargo's OUT_DIR.
/// Panics with a clear adapter diagnostic for unreadable inputs, incompatible anchors, or write failures.
fn main() {
    println!("cargo:rerun-if-env-changed=GLASSBOX_UPSTREAM_ROOT");
    let root = upstream_root();
    let input = root.join("crates/liteparse/src/main.rs");
    let manifest = root.join("crates/liteparse/Cargo.toml");
    println!("cargo:rerun-if-changed={}", input.display());
    println!("cargo:rerun-if-changed={}", manifest.display());
    let source = fs::read_to_string(&input)
        .unwrap_or_else(|error| panic!("read unchanged upstream {}: {error}", input.display()));
    let manifest_source = fs::read_to_string(&manifest)
        .unwrap_or_else(|error| panic!("read upstream manifest {}: {error}", manifest.display()));
    let version = upstream_version(&manifest_source);
    println!("cargo:rustc-env=GLASSBOX_UPSTREAM_VERSION={version}");
    let (glassbox, upstream_inventory) = generate_sources(&source);
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    fs::write(out.join("glassbox-upstream.rs"), glassbox)
        .expect("generate Glassbox upstream adapter");
    fs::write(out.join("upstream-inventory.rs"), upstream_inventory)
        .expect("generate upstream inventory probe");
    fs::write(out.join("upstream-main-copy.rs"), &source)
        .expect("retain exact adapter input in build output");
}

const ROUTER_SUPPORT: &str = r#"
/// Remove only the Glassbox outer parse token before an upstream command or alias.
///
/// Accepts no parameters and reads the process argument vector without Unicode conversion.
/// Returns every original OsString except a Glassbox argv0 and one structurally removed outer parse token.
/// Panics only if the process argument vector cannot be supplied by the Rust runtime.
fn glassbox_args() -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if !args.is_empty() {
        args[0] = std::ffi::OsString::from("glassbox");
    }
    if args.len() > 2 && args[1] == "parse" {
        let nested = &args[2];
        let is_upstream_command = Cli::command().get_subcommands().any(|command| {
            nested == std::ffi::OsStr::new(command.get_name())
                || command.get_all_aliases().any(|alias| nested == std::ffi::OsStr::new(alias))
        });
        if is_upstream_command {
            args.remove(1);
        }
    }
    args
}
"#;

const INVENTORY_SUPPORT: &str = r#"
/// Emit the complete generated Clap inventory when the verification environment requests it.
///
/// Accepts no parameters and reads GLASSBOX_PROBE_INVENTORY to select the diagnostic mode.
/// Returns unit during ordinary execution; diagnostic mode prints JSON and exits with status zero.
/// Exits with a Clap/serde diagnostic if command construction or inventory serialization fails.
fn glassbox_probe_inventory() {
    if std::env::var_os("GLASSBOX_PROBE_INVENTORY").is_none() {
        return;
    }
    let mut root = Cli::command();
    root.build();
    let commands: Vec<serde_json::Value> = root.get_subcommands().map(|command| {
        let arguments: Vec<serde_json::Value> = command.get_arguments().map(|argument| {
            serde_json::json!({
                "id": argument.get_id().as_str(),
                "long": argument.get_long(),
                "short": argument.get_short().map(|value| value.to_string()),
                "required": argument.is_required_set(),
                "action": format!("{:?}", argument.get_action()),
                "defaults": argument.get_default_values().iter().map(|value| value.to_string_lossy()).collect::<Vec<_>>(),
            })
        }).collect();
        serde_json::json!({
            "name": command.get_name(),
            "hidden": command.is_hide_set(),
            "aliases": command.get_all_aliases().collect::<Vec<_>>(),
            "arguments": arguments,
        })
    }).collect();
    println!("{}", serde_json::to_string_pretty(&commands).expect("inventory JSON"));
    std::process::exit(0);
}
"#;
