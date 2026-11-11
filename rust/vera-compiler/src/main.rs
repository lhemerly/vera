use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::process::ExitCode;

use vera_compiler::parse_source;

const HELP: &str = "VERA native frontend

Usage: vera parse <file|-> [--json]
       vera --help
       vera --version

parse    Parse the complete VERA syntax; --json emits the canonical syntax AST.
         Use '-' to read UTF-8 source from standard input.

This migration milestone parses syntax only. Native semantic checking,
verification, execution and code generation are not yet implemented.
";

fn run(args: &[OsString]) -> Result<(), (u8, String)> {
    if args == ["--help"] || args == ["-h"] || args == ["parse", "--help"] {
        print!("{HELP}");
        return Ok(());
    }
    if args == ["--version"] {
        println!("vera {} (native frontend)", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let Some(command) = args.first() else {
        return Err((2, HELP.into()));
    };
    if command != "parse" {
        return Err((
            2,
            format!(
                "unsupported native command {command:?}; this milestone implements only 'parse'"
            ),
        ));
    }
    let mut path = None;
    let mut json = false;
    let mut positional = false;
    for argument in &args[1..] {
        if !positional && argument == "--" {
            positional = true;
        } else if !positional && argument == "--json" {
            if json {
                return Err((2, "--json may be specified only once".into()));
            }
            json = true;
        } else if !positional && argument.to_string_lossy().starts_with('-') && argument != "-" {
            return Err((2, format!("unknown option {argument:?}")));
        } else if path.replace(argument.as_os_str()).is_some() {
            return Err((2, "parse accepts exactly one source file".into()));
        }
    }
    let path = path.ok_or_else(|| (2, "parse requires a source file or '-'".into()))?;
    let label = Path::new(path).display();
    let source = if path == OsStr::new("-") {
        let mut input = String::new();
        io::stdin()
            .read_to_string(&mut input)
            .map_err(|e| (1, format!("D_IO: stdin: {e}")))?;
        input
    } else {
        fs::read_to_string(Path::new(path)).map_err(|e| (1, format!("D_IO: {label}: {e}")))?
    };
    let module = parse_source(&source).map_err(|e| (1, format!("{label}:{e}")))?;
    let canonical = module.canonical_json();
    if json {
        println!(
            "{}",
            serde_json::to_string(&canonical).map_err(|e| (1, format!("D_JSON: {e}")))?
        );
    } else {
        let name = canonical["name"].as_str().unwrap_or("<anonymous>");
        let declarations = canonical["decls"]
            .as_array()
            .ok_or_else(|| (1, "D_AST: missing module declarations".into()))?;
        let count = |kind| {
            declarations
                .iter()
                .filter(|decl| decl["node"] == kind)
                .count()
        };
        println!(
            "Parsed module {name}: {} tables, {} transitions, {} invariants, {} effects, {} projections (syntax only)",
            count("TableDecl"),
            count("TransitionDecl"),
            count("InvariantDecl"),
            count("EffectDecl"),
            count("ProjectionDecl"),
        );
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(&env::args_os().skip(1).collect::<Vec<_>>()) {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, message)) => {
            eprintln!("{message}");
            ExitCode::from(code)
        }
    }
}
