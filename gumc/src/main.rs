pub mod lexer;
pub mod indent;
pub mod stdlib;
pub mod parser;
pub mod ast;
pub mod semantic;
pub mod codegen;

use clap::{Arg, ArgAction, Command};
use codegen::Backend;
use std::fs;
use std::path::Path;

// Assembles a Yul object into deployable EVM bytecode by driving solc
// --strict-assembly. gumc's own backend stops at Yul (the standard portable
// EVM IR); solc's battle-tested Yul->EVM pipeline (optimizer + assembler) is
fn assemble_yul(yul: &str, solc_path: &str) -> Result<String, String> {
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("gumc_{}.yul", std::process::id()));
    fs::write(&tmp, yul).map_err(|e| format!("could not write temp yul file: {}", e))?;

    let result = std::process::Command::new(solc_path)
        .arg("--strict-assembly")
        .arg("--optimize")
        .arg("--bin")
        .arg(&tmp)
        .output();
    let _ = fs::remove_file(&tmp);

    let output = result.map_err(|e| {
        format!(
            "could not run '{}': {}.\nInstall solc (https://github.com/ethereum/solidity/releases) and put it on PATH, or point at it with --solc <path>.",
            solc_path, e
        )
    })?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if !output.status.success() {
        return Err(format!("solc rejected the generated Yul:\n{}{}", stdout, stderr));
    }

    stdout
        .lines()
        .skip_while(|l| !l.starts_with("Binary representation"))
        .skip(1)
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("could not find bytecode in solc output:\n{}{}", stdout, stderr))
}

fn main() {
    let matches = Command::new("gumc")
        .about("The Gum Compiler")
        .arg(Arg::new("file").required(true).help("Path to a .gum source file"))
        .arg(
            Arg::new("bytecode")
                .long("bytecode")
                .action(ArgAction::SetTrue)
                .help("Assemble the generated Yul into deployable EVM bytecode (requires solc)"),
        )
        .arg(
            Arg::new("solc")
                .long("solc")
                .help("Path to the solc binary used for Yul assembly (default: 'solc' from PATH)"),
        )
        .arg(
            Arg::new("output")
                .long("output")
                .short('o')
                .help("With --bytecode: write the bytecode hex to this file instead of stdout"),
        )
        .arg(
            Arg::new("rich-reverts")
                .long("rich-reverts")
                .action(ArgAction::SetTrue)
                .help("Emit Solidity-style Panic(uint256) reason data on checked-arithmetic reverts (larger bytecode, decodable failures)"),
        )
        .arg(
            Arg::new("lock")
                .long("lock")
                .help("Storage-layout lockfile (JSON). Created on first use, then enforced: existing fields keep their slots across recompiles (upgrade-safe), new fields are appended, incompatible changes error. Run at deploy time and keep it in version control."),
        )
        .get_matches();

    let file_path = matches.get_one::<String>("file").unwrap();
    // Only local imports need a root, and the source file's own directory is the only sensible one. The standard library is compiled in, so nothing has to be found on disk for use gum. to work.
    let base_dir = Path::new(file_path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".to_string());

    let source_code = match fs::read_to_string(file_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Could not read '{}': {}", file_path, e);
            std::process::exit(1);
        }
    };

    println!("--> Compiling {}\n", file_path);

    match parser::parse_program(&source_code) {
        Ok(ast) => {
            let mut type_checker = semantic::TypeChecker::new();
            if let Err(errors) = type_checker.check(ast.clone(), &base_dir) {
                let n = errors.len();
                println!("\n{} semantic error{} found:", n, if n == 1 { "" } else { "s" });
                for (i, e) in errors.iter().enumerate() {
                    println!("  {}. {}", i + 1, e);
                }
                std::process::exit(1);
            }

            let mut backend = codegen::EvmYulBackend::with_lock(
                matches.get_flag("rich-reverts"),
                matches.get_one::<String>("lock").cloned(),
            );
            match backend.generate(&ast, &type_checker) {
                Ok(compiled_contracts) => {
                    for (name, yul, abi_json) in compiled_contracts {
                        println!("\n--- Contract: {} ---", name);
                        println!("\n{}", yul);
                        println!("\n--> [Codegen] {} ABI JSON Generated:\n{}", name, abi_json);

                        if matches.get_flag("bytecode") {
                            let solc = matches
                                .get_one::<String>("solc")
                                .map(String::as_str)
                                .unwrap_or("solc");
                            match assemble_yul(&yul, solc) {
                                Ok(hex) => {
                                    println!("\n--> [Assembler] {} EVM bytecode ({} bytes):", name, hex.len() / 2);
                                    if let Some(out_path) = matches.get_one::<String>("output") {
                                        let file_path = if out_path.ends_with(".bin") {
                                            out_path.replace(".bin", &format!("_{}.bin", name))
                                        } else {
                                            format!("{}_{}.bin", out_path, name)
                                        };
                                        if let Err(e) = fs::write(&file_path, format!("0x{}\n", hex)) {
                                            eprintln!("Could not write '{}': {}", file_path, e);
                                            std::process::exit(1);
                                        }
                                        println!("    written to {}", file_path);
                                    } else {
                                        println!("0x{}", hex);
                                    }
                                }
                                Err(e) => {
                                    eprintln!("Assembler Error for {}: {}", name, e);
                                    std::process::exit(1);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    println!("Codegen Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err(errors) => {
            let n = errors.len();
            println!("\n{} syntax error{} found:", n, if n == 1 { "" } else { "s" });
            for (i, e) in errors.iter().enumerate() {
                println!("  {}. {}", i + 1, e);
            }
            std::process::exit(1);
        }
    }
}
