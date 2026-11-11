"""VERA command line interface."""

import argparse
import json
import sys
from typing import Dict, List, Optional

from .errors import VeraError
from .parser import parse_file
from .semantic import analyze
from .smt_emit import (SmtConfig, verify_module, verify_transition,
                       verify_transition_dual)
from .verifier import attach_counterexample


def _load(path: str):
    module = parse_file(path)
    prog = analyze(module)
    return module, prog


def _print_diags(prog) -> bool:
    if prog.diags.items:
        print(prog.diags.render())
    return not prog.diags.has_errors()


def cmd_check(args) -> int:
    try:
        _, prog = _load(args.file)
    except VeraError as exc:
        print(exc.diag.render())
        return 1
    ok = _print_diags(prog)
    if ok:
        print(f"OK: {args.file} parsed and analyzed cleanly "
              f"({len(prog.tables)} tables, {len(prog.invariants)} invariants, "
              f"{len(prog.transitions)} transitions, "
              f"{len(prog.projections)} projections)")
        for name, ti in prog.transitions.items():
            print(f"  transition {name}: tier={ti.tier} wcet<={ti.wcet_bound} "
                  f"reads={len(ti.reads)} writes={len(ti.writes)} "
                  f"agg_writes={len(ti.agg_writes)}")
    return 0 if ok else 1


def cmd_verify(args) -> int:
    try:
        _, prog = _load(args.file)
    except VeraError as exc:
        print(exc.diag.render())
        return 1
    if not _print_diags(prog):
        return 1
    config = SmtConfig(timeout_ms=args.timeout, exact_sum=args.exact_sum)
    names = [args.transition] if args.transition else list(prog.transitions)
    all_ok = True
    for name in names:
        if name not in prog.transitions:
            print(f"error: unknown transition {name!r}")
            all_ok = False
            continue
        if args.exact_sum:
            result = verify_transition(prog, name, config)
            result.encoders = {"safety": None, "panic": None, "liveness": None}
            # single-mode: rebuild an encoder for the counterexample lifter
            from .smt_emit import SmtEncoder
            enc = SmtEncoder(prog, name, config)
            enc.build()
            result.encoders = {"safety": enc, "panic": enc, "liveness": enc}
        else:
            result = verify_transition_dual(prog, name, config)
        result = attach_counterexample(prog, result.encoders, result)
        print(result.summary())
        if result.counterexample:
            print(result.counterexample.render())
        if args.emit_smt:
            out = f"{args.emit_smt}.{name}.smt2"
            with open(out, "w", encoding="utf-8") as fh:
                fh.write(result.script)
            print(f"  SMT-LIB script written to {out}")
        if not result.ok:
            all_ok = False
    return 0 if all_ok else 1


def _parse_param(value: str, type_ref, prog):
    name = type_ref.name
    if name == "bool":
        return value.lower() in ("true", "1", "yes")
    if name == "id64":
        return int(value, 0)
    if name in ("u8", "u16", "u32", "u64", "i64"):
        return int(value, 0)
    if name in ("dec64", "dec128"):
        from .types import primitive
        ctype = primitive(name)
        # VERA literal convention: an integer literal is the scaled value; a
        # fractional literal is a human decimal.
        if "." in value:
            return int(round(float(value) * (10 ** ctype.scale)))
        return int(value, 0)
    if name == "sym32":
        from .smt_emit import _intern_symbol
        return _intern_symbol(value)
    if name == "bytes32":
        return bytes.fromhex(value.removeprefix("0x"))
    if name == "enum":
        tags = type_ref.enum_tags
        if value in tags:
            return tags.index(value)
        return int(value, 0)
    return value


def cmd_run(args) -> int:
    try:
        _, prog = _load(args.file)
    except VeraError as exc:
        print(exc.diag.render())
        return 1
    if not _print_diags(prog):
        return 1
    if args.transition not in prog.transitions:
        print(f"error: unknown transition {args.transition!r}")
        return 1
    from .runtime import Machine, Revert
    ti = prog.transitions[args.transition]
    params: Dict[str, object] = {}
    for spec in args.param or []:
        if "=" not in spec:
            print(f"error: parameter {spec!r} must be name=value")
            return 1
        key, value = spec.split("=", 1)
        param = next((p for p in ti.decl.params if p.name == key), None)
        if param is None:
            print(f"error: unknown parameter {key!r}")
            return 1
        params[key] = _parse_param(value, param.type, prog)
    missing = [p.name for p in ti.decl.params if p.name not in params]
    if missing:
        print(f"error: missing parameters: {', '.join(missing)}")
        return 1
    machine = Machine(prog)
    if args.setup:
        _apply_setup(machine, prog, args.setup)
    try:
        state, trace = machine.execute(args.transition, params, record_trace=True)
    except Revert as exc:
        print(f"REVERT({exc.code})")
        return 1
    print("COMMIT")
    for step in trace:
        print(f"  {step.text}")
    print("state:")
    for tname, t in state.tables.items():
        rows = []
        for slot in range(t.capacity):
            if not t.occ[slot]:
                continue
            key = (t.gen[slot] << t.key_bits) | slot
            row = {"id": key}
            for cname in prog.tables[tname].column_order:
                row[cname] = t.columns[cname][slot]
            rows.append(row)
        print(f"  {tname}: {rows}")
    if state.outbox:
        print("outbox:")
        for eff, argv in state.outbox:
            print(f"  {eff}({argv})")
    return 0


def _apply_setup(machine, prog, setup_path: str) -> None:
    """Apply a JSON setup file: {'tables': {name: [rows]}}."""
    with open(setup_path, "r", encoding="utf-8") as fh:
        data = json.load(fh)
    for tname, rows in data.get("tables", {}).items():
        t = machine.state.tables[tname]
        for row in rows:
            key = int(row["id"])
            slot = t.slot_of(key)
            t.occ[slot] = 1
            t.gen[slot] = key >> t.key_bits
            for cname in prog.tables[tname].column_order:
                t.columns[cname][slot] = row.get(cname, 0)
    machine.recompute_aggregates()


def cmd_codegen(args) -> int:
    try:
        _, prog = _load(args.file)
    except VeraError as exc:
        print(exc.diag.render())
        return 1
    if not _print_diags(prog):
        return 1
    if args.target == "rust":
        from .codegen_rust import generate_rust
        text = generate_rust(prog)
    elif args.target == "cuda":
        from .codegen_gpu import generate_cuda
        text = generate_cuda(prog)
    elif args.target == "wgsl":
        from .codegen_gpu import generate_wgsl
        text = generate_wgsl(prog)
    elif args.target == "glsl":
        from .codegen_glsl import generate_glsl
        text = generate_glsl(prog)
    else:
        print(f"error: unknown target {args.target!r}")
        return 1
    if args.out:
        with open(args.out, "w", encoding="utf-8") as fh:
            fh.write(text)
        print(f"wrote {args.target} source to {args.out} ({len(text)} bytes)")
    else:
        print(text)
    return 0


def cmd_fuzz(args) -> int:
    try:
        _, prog = _load(args.file)
    except VeraError as exc:
        print(exc.diag.render())
        return 1
    if not _print_diags(prog):
        return 1
    from .fuzz import fuzz_module
    result = fuzz_module(prog, steps=args.steps, threads=args.threads,
                         seed=args.seed, capacity=args.capacity)
    print(result.render())
    return 0 if result.ok else 1


def cmd_project(args) -> int:
    try:
        _, prog = _load(args.file)
    except VeraError as exc:
        print(exc.diag.render())
        return 1
    if not _print_diags(prog):
        return 1
    from .projection import build_affordance_frame
    from .runtime import Machine, Revert
    if args.projection not in prog.projections:
        print(f"error: unknown projection {args.projection!r}")
        return 1
    machine = Machine(prog)
    if args.setup:
        _apply_setup(machine, prog, args.setup)
    params: Dict[str, object] = {}
    for spec in args.param or []:
        key, value = spec.split("=", 1)
        params[key] = _parse_param(value, next(p.type for p in prog.projections[args.projection].decl.params if p.name == key), prog)
    try:
        frame = build_affordance_frame(prog, machine.state, args.projection, params)
    except Revert as exc:
        print(f"projection reverted: {exc.code}")
        return 1
    print(json.dumps(frame.to_json(), indent=2, sort_keys=True))
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="vera", description="VERA v1.1.0 toolchain")
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("check", help="parse and analyze a VERA module")
    p.add_argument("file")
    p.set_defaults(func=cmd_check)

    p = sub.add_parser("verify", help="run the three proof obligations")
    p.add_argument("file")
    p.add_argument("--transition")
    p.add_argument("--timeout", type=int, default=5000)
    p.add_argument("--exact-sum", action="store_true")
    p.add_argument("--emit-smt")
    p.set_defaults(func=cmd_verify)

    p = sub.add_parser("run", help="execute a transition in the reference runtime")
    p.add_argument("file")
    p.add_argument("--transition", required=True)
    p.add_argument("--param", action="append", default=[])
    p.add_argument("--setup")
    p.set_defaults(func=cmd_run)

    p = sub.add_parser("codegen", help="emit a compilation target")
    p.add_argument("target", choices=["rust", "cuda", "wgsl", "glsl"])
    p.add_argument("file")
    p.add_argument("--out")
    p.set_defaults(func=cmd_codegen)

    p = sub.add_parser("fuzz", help="run the reference trajectory fuzzer")
    p.add_argument("file")
    p.add_argument("--steps", type=int, default=64)
    p.add_argument("--threads", type=int, default=256)
    p.add_argument("--seed", type=int, default=12345)
    p.add_argument("--capacity", type=int, default=16)
    p.set_defaults(func=cmd_fuzz)

    p = sub.add_parser("project", help="evaluate a projection affordance frame")
    p.add_argument("file")
    p.add_argument("--projection", required=True)
    p.add_argument("--param", action="append", default=[])
    p.add_argument("--setup")
    p.set_defaults(func=cmd_project)
    return parser


def main(argv: Optional[List[str]] = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
