use num_bigint::BigInt;
use serde_json::{json, Value as Json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read},
    path::Path,
};
use vera_compiler::{
    codegen_gpu::{generate_gpu, GpuTarget},
    codegen_rust::{generate_rust, RustOptions},
    fuzz::{fuzz_module, FuzzConfig},
    ir::{CheckedProgram, Param},
    parse_source,
    persistence::{self, Intent, Wal},
    projection::build_affordance_frame,
    runtime::{rebuild_indexes, recompute_aggregates, Machine, State},
    semantic::analyze,
    types::{ScalarType, ValueType},
    value::{self, Value},
    verifier::{verify_all, verify_function, verify_transition, VerifierConfig},
};

type Result<T> = std::result::Result<T, (u8, String)>;
const HELP: &str = "VERA native compiler

Usage: vera <command> <file|-> [options]

  parse     Parse syntax; --json emits the canonical syntax AST
  check     Resolve types, footprints, bounds and declarations
  verify    Prove safety, panic freedom and concrete non-vacuity
  run       Execute --transition NAME [--param NAME=VALUE ...]
  project   Read --projection NAME [--param NAME=VALUE ...]
  replay    Recover --wal PATH from the initial --setup state
  codegen   Emit rust|glsl|wgsl|cuda: vera codegen TARGET FILE
  fuzz      Search native execution traces for violations

Common: --json, --setup FILE
Proofs: --transition NAME or --function NAME, --emit-smt PREFIX, --exact-sum
Optional proof budgets (all unset by default): --timeout MS,
        --max-state-cells N, --max-unroll N, --witness-attempts N,
        --max-expression-steps N, --max-smt-bytes N, --max-effect-events N
Execution: --unverified (development), --snapshot-in FILE,
           --snapshot-out FILE, --wal FILE (recover, append and sync)
Codegen: --out FILE, --no-main, --runtime-crate, --depth N
Fuzzing: --steps N, --threads N, --seed N, --capacity N (seed row limit)

All commands are native. The linked Z3 solver proves run/codegen/replay
before use unless --unverified is explicitly supplied. Unknown results and
timeouts reject compilation. Snapshots have no WAL cursor: --snapshot-in
cannot be combined with --wal. Use '-' for UTF-8 stdin, '--' to end options.
";
fn input(message: impl Into<String>) -> (u8, String) {
    (2, message.into())
}
fn failure(message: impl std::fmt::Display) -> (u8, String) {
    (1, message.to_string())
}

struct Options {
    command: String,
    positional: Vec<OsString>,
    values: BTreeMap<String, OsString>,
    flags: BTreeSet<String>,
    params: Vec<OsString>,
}
impl Options {
    fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }
    fn text(&self, name: &str) -> Result<Option<&str>> {
        self.values
            .get(name)
            .map(|s| {
                s.to_str()
                    .ok_or_else(|| input(format!("{name} requires UTF-8 text")))
            })
            .transpose()
    }
    fn required(&self, name: &str) -> Result<&str> {
        self.text(name)?
            .ok_or_else(|| input(format!("{} requires {name}", self.command)))
    }
    fn number<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T> {
        self.text(name)?
            .map(|s| {
                s.parse()
                    .map_err(|_| input(format!("invalid {name}: {s:?}")))
            })
            .unwrap_or(Ok(default))
    }
    fn optional_number<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>> {
        self.text(name)?
            .map(|s| {
                s.parse()
                    .map_err(|_| input(format!("invalid {name}: {s:?}")))
            })
            .transpose()
    }
    fn config(&self) -> Result<VerifierConfig> {
        let config = VerifierConfig {
            timeout_ms: self.optional_number("--timeout")?,
            max_state_cells: self.optional_number("--max-state-cells")?,
            max_unroll: self.optional_number("--max-unroll")?,
            witness_attempts: self.optional_number("--witness-attempts")?,
            max_expression_steps: self.optional_number("--max-expression-steps")?,
            max_smt_bytes: self.optional_number("--max-smt-bytes")?,
            max_effect_events: self.optional_number("--max-effect-events")?,
        };
        if config.timeout_ms == Some(0) {
            return Err(input("--timeout must be positive"));
        }
        Ok(config)
    }
}
fn options(mut args: Vec<OsString>) -> Result<Options> {
    let command = args
        .remove(0)
        .into_string()
        .map_err(|_| input("command requires UTF-8 text"))?;
    let (flags, values): (&[&str], &[&str]) = match command.as_str() {
        "parse" | "check" => (&["--json"], &[]),
        "verify" => (
            &["--json", "--exact-sum"],
            &[
                "--transition",
                "--function",
                "--timeout",
                "--emit-smt",
                "--max-state-cells",
                "--max-unroll",
                "--witness-attempts",
                "--max-expression-steps",
                "--max-smt-bytes",
                "--max-effect-events",
            ],
        ),
        "run" => (
            &["--json", "--unverified"],
            &[
                "--transition",
                "--param",
                "--setup",
                "--timeout",
                "--snapshot-in",
                "--snapshot-out",
                "--wal",
                "--max-state-cells",
                "--max-unroll",
                "--witness-attempts",
                "--max-expression-steps",
                "--max-smt-bytes",
                "--max-effect-events",
            ],
        ),
        "project" => (
            &["--json"],
            &["--projection", "--param", "--setup", "--snapshot-in"],
        ),
        "replay" => (
            &["--json", "--unverified"],
            &[
                "--setup",
                "--wal",
                "--snapshot-out",
                "--timeout",
                "--max-state-cells",
                "--max-unroll",
                "--witness-attempts",
                "--max-expression-steps",
                "--max-smt-bytes",
                "--max-effect-events",
            ],
        ),
        "codegen" => (
            &["--unverified", "--no-main", "--runtime-crate"],
            &[
                "--out",
                "--timeout",
                "--depth",
                "--max-state-cells",
                "--max-unroll",
                "--witness-attempts",
                "--max-expression-steps",
                "--max-smt-bytes",
                "--max-effect-events",
            ],
        ),
        "fuzz" => (
            &["--json"],
            &["--steps", "--threads", "--seed", "--capacity"],
        ),
        _ => return Err(input(format!("unknown command {command:?}\n{HELP}"))),
    };
    let mut opts = Options {
        command,
        positional: Vec::new(),
        values: BTreeMap::new(),
        flags: BTreeSet::new(),
        params: Vec::new(),
    };
    let mut args = args.into_iter();
    let mut positional = false;
    while let Some(arg) = args.next() {
        if !positional && arg == "--" {
            positional = true;
            continue;
        }
        if !positional && arg != "-" && arg.to_string_lossy().starts_with('-') {
            let key = arg
                .to_str()
                .ok_or_else(|| input("option requires UTF-8 text"))?;
            if flags.contains(&key) {
                if !opts.flags.insert(key.to_owned()) {
                    return Err(input(format!("{key} supplied twice")));
                }
            } else if values.contains(&key) {
                let value = args
                    .next()
                    .ok_or_else(|| input(format!("{key} requires a value")))?;
                if key == "--param" {
                    opts.params.push(value);
                } else if opts.values.insert(key.to_owned(), value).is_some() {
                    return Err(input(format!("{key} supplied twice")));
                }
            } else {
                return Err(input(format!(
                    "unknown option {key:?} for {}",
                    opts.command
                )));
            }
        } else {
            opts.positional.push(arg);
        }
    }
    let count = if opts.command == "codegen" { 2 } else { 1 };
    if opts.positional.len() != count {
        return Err(input(format!(
            "{} requires {}source file or '-'",
            opts.command,
            if count == 2 {
                "a target and one "
            } else {
                "exactly one "
            }
        )));
    }
    Ok(opts)
}
fn source(path: &OsStr) -> Result<String> {
    if path == OsStr::new("-") {
        let mut text = String::new();
        io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| failure(format!("D_IO: stdin: {e}")))?;
        Ok(text)
    } else {
        fs::read_to_string(path)
            .map_err(|e| failure(format!("D_IO: {}: {e}", Path::new(path).display())))
    }
}
fn print_json(value: impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&value).map_err(failure)?);
    Ok(())
}
fn integer(text: &str, ty: &ScalarType) -> std::result::Result<Value, String> {
    let (neg, body) = text
        .strip_prefix('-')
        .map(|s| (true, s))
        .unwrap_or((false, text));
    let (base, body) = if let Some(s) = body.strip_prefix("0x") {
        (16, s)
    } else if let Some(s) = body.strip_prefix("0b") {
        (2, s)
    } else if let Some(s) = body.strip_prefix("0o") {
        (8, s)
    } else {
        (10, body)
    };
    let n =
        BigInt::parse_bytes(body.as_bytes(), base).ok_or_else(|| "invalid integer".to_owned())?;
    value::coerce(&Value::Int(if neg { -n } else { n }), ty)
}
fn parameter(text: &str, ty: &ScalarType) -> std::result::Result<Value, String> {
    match ty {
        ScalarType::Bool => match text.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Ok(Value::Bool(true)),
            "false" | "no" | "0" => Ok(Value::Bool(false)),
            _ => Err("expected true or false".into()),
        },
        ScalarType::Sym32 | ScalarType::Enum(_) => {
            value::from_json(ty, &Json::String(text.into())).or_else(|_| integer(text, ty))
        }
        ScalarType::Bytes32 => value::from_json(
            ty,
            &Json::String(if text.starts_with("0x") {
                text.into()
            } else {
                format!("0x{text}")
            }),
        ),
        ScalarType::Dec64 | ScalarType::Dec128 if text.contains(['.', 'e', 'E']) => {
            value::from_json(ty, &Json::String(text.into()))
        }
        _ => integer(text, ty),
    }
}
fn parameters(
    program: &CheckedProgram,
    specs: &[OsString],
    params: &[Param],
) -> Result<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    for spec in specs {
        let text = spec
            .to_str()
            .ok_or_else(|| input("parameters require UTF-8 text"))?;
        let (name, text) = text
            .split_once('=')
            .ok_or_else(|| input("parameters must be NAME=VALUE"))?;
        let param = params
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| input(format!("unknown parameter {name:?}")))?;
        let value = match &param.ty {
            ValueType::Scalar(ty) => parameter(text, ty),
            ValueType::Bytes { .. } => {
                value::value_from_json(program, &Json::String(text.into()), &param.ty)
            }
            ValueType::Named(_) => serde_json::from_str::<Json>(text)
                .map_err(|e| e.to_string())
                .and_then(|json| value::value_from_json(program, &json, &param.ty)),
        }
        .map_err(|e| input(format!("{name}: {e}")))?;
        if out.insert(name.into(), value).is_some() {
            return Err(input(format!("parameter {name:?} supplied twice")));
        }
    }
    let missing: Vec<_> = params
        .iter()
        .filter(|p| !out.contains_key(&p.name))
        .map(|p| p.name.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(input(format!("missing parameters: {}", missing.join(", "))));
    }
    Ok(out)
}
fn initial_state(program: &CheckedProgram, opts: &Options) -> Result<State> {
    if opts.values.contains_key("--snapshot-in")
        && (opts.values.contains_key("--setup") || opts.values.contains_key("--wal"))
    {
        return Err(input(
            "--snapshot-in cannot be combined with --setup or --wal; snapshots have no log cursor",
        ));
    }
    if let Some(path) = opts.values.get("--snapshot-in") {
        return persistence::decode_state(program, &fs::read(path).map_err(failure)?)
            .map_err(failure);
    }
    let mut state = State::empty(program).map_err(failure)?;
    if let Some(path) = opts.values.get("--setup") {
        let setup: Json =
            serde_json::from_slice(&fs::read(path).map_err(failure)?).map_err(failure)?;
        let object = setup
            .as_object()
            .ok_or_else(|| input("setup must be an object"))?;
        if object.keys().any(|k| k != "tables") {
            return Err(input("unknown setup field"));
        }
        let tables = object
            .get("tables")
            .and_then(Json::as_object)
            .ok_or_else(|| input("setup.tables must be an object"))?;
        for (name, rows) in tables {
            let id = program
                .table_id(name)
                .ok_or_else(|| input(format!("unknown setup table {name:?}")))?;
            let info = program.table(id);
            let target = &mut state.tables[id.0];
            let mut seen = BTreeSet::new();
            for row in rows
                .as_array()
                .ok_or_else(|| input("setup table must be an array"))?
            {
                let row = row
                    .as_object()
                    .ok_or_else(|| input("setup row must be an object"))?;
                let key = row
                    .get("id")
                    .and_then(Json::as_u64)
                    .ok_or_else(|| input("setup handle must be an unsigned 64-bit integer"))?;
                let bits = info.key_bits() + info.generation_bits();
                if bits < 64 && key >> bits != 0 {
                    return Err(input("invalid setup generation bits"));
                }
                let slot = (key & (info.capacity - 1)) as usize;
                if !seen.insert(slot) {
                    return Err(input("setup assigns a slot twice"));
                }
                let nested = row.get("columns").and_then(Json::as_object);
                let columns = nested.unwrap_or(row);
                if nested.is_some() && row.keys().any(|k| k != "id" && k != "columns") {
                    return Err(input("nested setup row accepts id and columns"));
                }
                if columns
                    .keys()
                    .any(|k| (nested.is_some() || k != "id") && info.column_id(k).is_none())
                {
                    return Err(input("unknown setup column"));
                }
                target.occupied[slot] = true;
                target.generations[slot] = (key >> info.key_bits()) as u32;
                for column in &info.columns {
                    if nested.is_none() && column.name == "id" {
                        continue;
                    }
                    if let Some(v) = columns.get(&column.name) {
                        target.columns[column.id.index][slot] =
                            value::value_from_json(program, v, &column.ty).map_err(input)?;
                    }
                }
            }
        }
        recompute_aggregates(program, &mut state).map_err(failure)?;
        rebuild_indexes(program, &mut state).map_err(failure)?;
    }
    vera_compiler::runtime::validate_state(program, &state).map_err(failure)?;
    Ok(state)
}
fn state_json(program: &CheckedProgram, state: &State) -> Json {
    let tables: serde_json::Map<String, Json> = program
        .tables()
        .iter()
        .map(|info| {
            let data = &state.tables[info.id.0];
            let rows: Vec<_> = (0..data.occupied.len())
                .filter_map(|slot| {
                    data.key_at(info, slot).map(|key| {
                        let mut row = serde_json::Map::new();
                        row.insert("id".into(), json!(key));
                        let mut columns = serde_json::Map::new();
                        for c in &info.columns {
                            columns.insert(
                                c.name.clone(),
                                value::value_to_json(
                                    program,
                                    &data.columns[c.id.index][slot],
                                    &c.ty,
                                ),
                            );
                        }
                        if info.column_id("id").is_some() {
                            row.insert("columns".into(), Json::Object(columns));
                        } else {
                            row.extend(columns);
                        }
                        Json::Object(row)
                    })
                })
                .collect();
            (info.name.clone(), Json::Array(rows))
        })
        .collect();
    let outbox: Vec<_> = state
        .outbox
        .iter()
        .map(|record| {
            let effect = program.effect(record.effect);
            let args: serde_json::Map<String, Json> = effect
                .params
                .iter()
                .zip(&record.args)
                .map(|(p, v)| (p.name.clone(), value::value_to_json(program, v, &p.ty)))
                .collect();
            json!({"effect":effect.name,"args":args})
        })
        .collect();
    json!({"tables":tables,"outbox":outbox})
}
fn proof_gate(program: &CheckedProgram, opts: &Options, names: &[&str], all: bool) -> Result<()> {
    if opts.flag("--unverified") {
        return Ok(());
    }
    let config = opts.config()?;
    let results = if all {
        verify_all(program, &config)
    } else {
        names
            .iter()
            .map(|name| verify_transition(program, name, &config))
            .collect()
    };
    for result in results {
        if !result.ok() {
            return Err(failure(format!(
                "Compilation rejected: {} {:?} is not verified.\n{}",
                result.target_kind,
                result.transition,
                result.summary()
            )));
        }
    }
    Ok(())
}
fn paths_alias(left: &OsStr, right: &OsStr) -> bool {
    #[cfg(unix)]
    if let (Ok(a), Ok(b)) = (fs::metadata(left), fs::metadata(right)) {
        use std::os::unix::fs::MetadataExt;
        if a.dev() == b.dev() && a.ino() == b.ino() {
            return true;
        }
    }
    let resolve = |path: &OsStr| {
        let path = Path::new(path);
        fs::canonicalize(path).or_else(|_| {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            fs::canonicalize(parent).map(|p| p.join(path.file_name().unwrap_or_default()))
        })
    };
    matches!((resolve(left), resolve(right)), (Ok(a), Ok(b)) if a == b)
}

fn validate_outputs(opts: &Options) -> Result<()> {
    if let (Some(wal), Some(snapshot)) =
        (opts.values.get("--wal"), opts.values.get("--snapshot-out"))
    {
        if paths_alias(wal, snapshot) {
            return Err(input("--snapshot-out must not alias the WAL"));
        }
    }
    Ok(())
}

pub fn run(args: Vec<OsString>) -> Result<()> {
    if args.is_empty()
        || args
            .iter()
            .take_while(|s| *s != "--")
            .any(|s| s == "--help" || s == "-h")
    {
        print!("{HELP}");
        return Ok(());
    }
    if args == ["--version"] {
        println!("vera {} (native)", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let opts = options(args)?;
    validate_outputs(&opts)?;
    let path = opts.positional.last().expect("validated source");
    let module = parse_source(&source(path)?)
        .map_err(|e| failure(format!("{}:{e}", Path::new(path).display())))?;
    if opts.command == "parse" {
        if opts.flag("--json") {
            return print_json(module.canonical_json());
        }
        let j = module.canonical_json();
        println!(
            "Parsed module {}: {} declarations",
            j["name"].as_str().unwrap_or("<anonymous>"),
            j["decls"].as_array().map_or(0, Vec::len)
        );
        return Ok(());
    }
    let program = analyze(module).map_err(|errors| {
        failure(
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    })?;
    let all_names: Vec<_> = program
        .transitions()
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    match opts.command.as_str() {
        "check" => {
            let tables:Vec<_>=program.tables().iter().map(|t|json!({"name":t.name,"capacity":t.capacity,"bytes":t.layout.size,"columns":t.columns.iter().map(|c|json!({"name":c.name,"type":program.value_type_name(&c.ty),"tracked_sum":c.tracked_sum,"unique":c.unique})).collect::<Vec<_>>()})).collect();
            let transitions:Vec<_>=program.transitions().iter().map(|t|json!({"name":t.name,"tier":t.tier,"wcet_bound":t.wcet_bound,"reads":t.reads.len(),"writes":t.writes.len(),"fold_depth":t.fold_depth,"max_fold_product":t.max_fold_product})).collect();
            let functions:Vec<_>=program.functions().iter().map(|f|json!({"name":f.name,"result_type":program.value_type_name(&f.result_type),"wcet_bound":f.wcet_bound,"dependencies":f.dependencies.iter().map(|id|program.function(*id).name.clone()).collect::<Vec<_>>()})).collect();
            if opts.flag("--json") {
                let mut report = json!({"ok":true,"tables":tables,"transitions":transitions,"invariants":program.invariants().len(),"projections":program.projections().len()});
                if !functions.is_empty() {
                    report["functions"] = json!(functions);
                }
                print_json(report)?;
            } else {
                println!(
                    "OK: {} tables, {} transitions, {} invariants, {} projections",
                    tables.len(),
                    transitions.len(),
                    program.invariants().len(),
                    program.projections().len()
                );
                for t in transitions {
                    println!(
                        "  {}: tier={} wcet<={}",
                        t["name"], t["tier"], t["wcet_bound"]
                    );
                }
            }
        }
        "verify" => {
            let config = opts.config()?;
            let selected_transition = opts.text("--transition")?;
            let selected_function = opts.text("--function")?;
            if selected_transition.is_some() && selected_function.is_some() {
                return Err(input("select --transition or --function, not both"));
            }
            let results = if let Some(name) = selected_transition {
                if program.transition_id(name).is_none() {
                    return Err(input("unknown transition"));
                }
                vec![verify_transition(&program, name, &config)]
            } else if let Some(name) = selected_function {
                if program.function_id(name).is_none() {
                    return Err(input("unknown function"));
                }
                vec![verify_function(&program, name, &config)]
            } else {
                verify_all(&program, &config)
            };
            for r in &results {
                if let Some(prefix) = opts.values.get("--emit-smt") {
                    for (kind, script) in [
                        ("safety", &r.smt_safety),
                        ("panic", &r.smt_panic),
                        ("liveness", &r.smt_liveness),
                    ] {
                        let mut path = prefix.clone();
                        if r.target_kind == "function" {
                            path.push(format!(".function.{}.{kind}.smt2", r.transition));
                        } else {
                            path.push(format!(".{}.{kind}.smt2", r.transition));
                        }
                        fs::write(path, script).map_err(failure)?;
                    }
                }
                if !opts.flag("--json") {
                    println!("{}", r.summary());
                    if let Some(c) = &r.counterexample {
                        print_json(c)?;
                    }
                }
            }
            let ok = results.iter().all(|r| r.ok());
            if opts.flag("--json") {
                print_json(json!({"ok":ok,"results":results}))?;
            }
            if !ok {
                return Err((1, String::new()));
            }
        }
        "run" | "replay" | "project" => {
            let transition = if opts.command == "run" {
                Some(opts.required("--transition")?)
            } else {
                None
            };
            let projection = if opts.command == "project" {
                Some(opts.required("--projection")?)
            } else {
                None
            };
            let params = if let Some(n) = transition {
                &program
                    .transition(
                        program
                            .transition_id(n)
                            .ok_or_else(|| input("unknown transition"))?,
                    )
                    .params[..]
            } else if let Some(n) = projection {
                &program
                    .projection(
                        program
                            .projection_id(n)
                            .ok_or_else(|| input("unknown projection"))?,
                    )
                    .params[..]
            } else {
                &[]
            };
            let args = parameters(&program, &opts.params, params)?;
            if opts.command != "project" {
                proof_gate(
                    &program,
                    &opts,
                    &if let Some(n) = transition.filter(|_| !opts.values.contains_key("--wal")) {
                        vec![n]
                    } else {
                        all_names
                    },
                    transition.is_none() || opts.values.contains_key("--wal"),
                )?;
            }
            let mut machine =
                Machine::with_state(&program, initial_state(&program, &opts)?).map_err(failure)?;
            if let Some(n) = projection {
                return print_json(
                    build_affordance_frame(&program, &machine.state, n, &args)
                        .map_err(failure)?
                        .to_json(),
                );
            }
            if opts.command == "replay" {
                opts.required("--wal")?;
            }
            let mut wal = if let Some(path) = opts.text("--wal")? {
                let wal = Wal::open(path).map_err(failure)?;
                persistence::replay(&mut machine, path).map_err(failure)?;
                Some(wal)
            } else {
                None
            };
            let outcome = if let Some(n) = transition {
                if let Some(wal) = &mut wal {
                    let intent = Intent::from_parameters(&program, n, &args).map_err(failure)?;
                    persistence::execute_logged_traced(&mut machine, wal, &intent, true)
                        .map_err(failure)?
                } else {
                    machine.execute(n, &args, true).map(|_| ())
                }
            } else {
                Ok(())
            };
            if let Some(path) = opts.text("--snapshot-out")? {
                vera_rt::atomic_write(
                    path,
                    &persistence::encode_state(&program, &machine.state).map_err(failure)?,
                )
                .map_err(failure)?;
            }
            let ok = outcome.is_ok();
            if opts.flag("--json") {
                print_json(
                    json!({"ok":ok,"error":outcome.as_ref().err(),"state":state_json(&program,&machine.state),"trace":machine.last_trace}),
                )?;
            } else {
                println!(
                    "{}",
                    if ok {
                        "COMMIT".into()
                    } else {
                        format!("REVERT({})", outcome.as_ref().unwrap_err())
                    }
                );
                print_json(state_json(&program, &machine.state))?;
            }
            if !ok {
                return Err((1, String::new()));
            }
        }
        "codegen" => {
            let target = opts.positional[0]
                .to_str()
                .ok_or_else(|| input("target requires UTF-8 text"))?;
            if !["rust", "glsl", "wgsl", "cuda"].contains(&target) {
                return Err(input("target must be rust, glsl, wgsl or cuda"));
            }
            if target != "rust" && (opts.flag("--no-main") || opts.flag("--runtime-crate")) {
                return Err(input("--no-main and --runtime-crate apply to Rust"));
            }
            proof_gate(&program, &opts, &all_names, true)?;
            let text = if target == "rust" {
                generate_rust(
                    &program,
                    RustOptions {
                        use_rt: opts.flag("--runtime-crate"),
                        include_main: !opts.flag("--no-main"),
                    },
                )
            } else {
                generate_gpu(
                    &program,
                    match target {
                        "glsl" => GpuTarget::Glsl,
                        "wgsl" => GpuTarget::Wgsl,
                        _ => GpuTarget::Cuda,
                    },
                    opts.number("--depth", 64)?,
                )
            }
            .map_err(failure)?;
            if let Some(path) = opts.values.get("--out") {
                fs::write(path, text).map_err(failure)?;
            } else {
                print!("{text}");
            }
        }
        "fuzz" => {
            let config = FuzzConfig {
                steps: opts.number("--steps", 64)?,
                threads: opts.number("--threads", 256)?,
                seed: opts.number("--seed", 12345)?,
                seed_rows: opts
                    .text("--capacity")?
                    .map(|s| s.parse().map_err(|_| input("invalid seed row limit")))
                    .transpose()?,
            };
            let result = fuzz_module(&program, &config).map_err(failure)?;
            let ok = result.ok;
            if opts.flag("--json") {
                print_json(&result)?;
            } else {
                println!("VERA native furnace: {} threads x {} steps\n  executed: {}\n  committed: {}\n  violations: {}",result.threads,result.steps,result.executed,result.committed,result.violations.len());
                if !ok {
                    print_json(&result.violations)?;
                }
            }
            if !ok {
                return Err((1, String::new()));
            }
        }
        _ => unreachable!("validated command"),
    }
    Ok(())
}
