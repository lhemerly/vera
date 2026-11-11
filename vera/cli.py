"""VERA command line interface."""

import argparse
import json
import sys
from decimal import Decimal
from typing import Dict, List, Optional

from .errors import VeraError
from .parser import parse_file
from .semantic import analyze
from .smt_emit import (
    SmtConfig,
    verify_module,
    verify_transition,
    verify_transition_dual,
)
from .verifier import attach_counterexample


def _load(path: str):
    module = parse_file(path)
    prog = analyze(module)
    return module, prog


def _print_diags(prog, *, file=None) -> bool:
    if prog.diags.items:
        print(prog.diags.render(), file=file)
    return not prog.diags.has_errors()


def _proof_gate(prog, args, names=None) -> bool:
    """Only publish/run a program whose obligations were discharged."""
    if getattr(args, "unverified", False):
        return True
    config = SmtConfig(timeout_ms=args.timeout)
    accepted = True
    for name in names if names is not None else prog.transitions:
        result = verify_transition_dual(prog, name, config)
        if not result.ok:
            print(
                f"Compilation rejected: transition {name!r} is not verified.",
                file=sys.stderr,
            )
            print(result.summary(), file=sys.stderr)
            for diagnostic in result.diagnostics:
                print(diagnostic.render(), file=sys.stderr)
            accepted = False
    return accepted


def cmd_check(args) -> int:
    try:
        _, prog = _load(args.file)
    except VeraError as exc:
        print(exc.diag.render())
        return 1
    ok = _print_diags(prog)
    if ok:
        print(
            f"OK: {args.file} parsed and analyzed cleanly "
            f"({len(prog.tables)} tables, {len(prog.invariants)} invariants, "
            f"{len(prog.transitions)} transitions, "
            f"{len(prog.projections)} projections)"
        )
        for name, ti in prog.transitions.items():
            print(
                f"  transition {name}: tier={ti.tier} wcet<={ti.wcet_bound} "
                f"reads={len(ti.reads)} writes={len(ti.writes)} "
                f"agg_writes={len(ti.agg_writes)}"
            )
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
        for diagnostic in result.diagnostics:
            print(diagnostic.render())
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
    from .semantic import _resolve_type_ref
    from .types import literal_value, symbol_id

    name = type_ref.name
    ctype = _resolve_type_ref(prog, type_ref, "command line parameter")
    if name == "bool":
        normalized = value.lower()
        if normalized not in ("true", "false", "1", "0", "yes", "no"):
            raise ValueError(f"invalid boolean {value!r}; expected true or false")
        return normalized in ("true", "1", "yes")
    elif name in ("dec64", "dec128") and "." in value:
        return literal_value(Decimal(value), ctype)
    elif name == "sym32":
        return symbol_id(value)
    elif name == "bytes32":
        parsed = bytes.fromhex(value.removeprefix("0x"))
        if len(parsed) != 32:
            raise ValueError("bytes32 parameters must contain exactly 32 bytes")
        return parsed
    elif name == "enum":
        tags = type_ref.enum_tags
        if value in tags:
            return tags.index(value)
    parsed = int(value, 0)
    if ctype is None or not ctype.min_value <= parsed <= ctype.max_value:
        raise ValueError(
            f"parameter {value!r} is outside the domain of {type_ref.name}"
        )
    return parsed


def _parse_params(specs, declarations, prog):
    declarations = {p.name: p for p in declarations}
    params = {}
    for spec in specs:
        if "=" not in spec:
            raise ValueError(f"parameter {spec!r} must be name=value")
        name, value = spec.split("=", 1)
        if name not in declarations:
            raise ValueError(f"unknown parameter {name!r}")
        if name in params:
            raise ValueError(f"parameter {name!r} supplied twice")
        params[name] = _parse_param(value, declarations[name].type, prog)
    missing = declarations.keys() - params.keys()
    if missing:
        raise ValueError(f"missing parameters: {', '.join(sorted(missing))}")
    return params


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
    params = _parse_params(args.param or [], ti.decl.params, prog)
    if not _proof_gate(prog, args, [args.transition]):
        return 1
    machine = Machine(prog)
    if args.setup:
        _apply_setup(machine, prog, args.setup)
    machine.validate_state()
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
        if tname not in prog.tables:
            raise ValueError(f"unknown setup table {tname!r}")
        t = machine.state.tables[tname]
        seen = set()
        for row in rows:
            key = int(row["id"])
            if key < 0 or key >= (1 << (t.key_bits + 32)):
                raise ValueError(f"invalid setup handle {key} for {tname}")
            slot = t.slot_of(key)
            if slot in seen:
                raise ValueError(f"setup assigns {tname} slot {slot} twice")
            seen.add(slot)
            unknown = row.keys() - {"id", *prog.tables[tname].column_order}
            if unknown:
                raise ValueError(f"unknown setup columns: {', '.join(sorted(unknown))}")
            t.occ[slot] = 1
            t.gen[slot] = key >> t.key_bits
            for cname in prog.tables[tname].column_order:
                ctype = prog.tables[tname].columns[cname].type
                default = (
                    False
                    if ctype.is_bool
                    else bytes(32)
                    if ctype.kind == "bytes32"
                    else 0
                )
                value = row.get(cname, default)
                if ctype.kind == "bytes32" and isinstance(value, str):
                    value = bytes.fromhex(value.removeprefix("0x"))
                t.columns[cname][slot] = value
    machine.recompute_aggregates()
    machine.rebuild_indexes()


def cmd_codegen(args) -> int:
    try:
        _, prog = _load(args.file)
    except VeraError as exc:
        print(exc.diag.render(), file=sys.stderr)
        return 1
    if not _print_diags(prog, file=sys.stderr):
        return 1
    if not _proof_gate(prog, args):
        return 1
    if args.target == "rust":
        from .codegen_rust import generate_rust

        text = generate_rust(
            prog, use_rt=args.runtime_crate, include_main=not args.no_main
        )
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

    result = fuzz_module(
        prog,
        steps=args.steps,
        threads=args.threads,
        seed=args.seed,
        capacity=args.capacity,
    )
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
    params = _parse_params(
        args.param or [], prog.projections[args.projection].decl.params, prog
    )
    machine.validate_state()
    try:
        frame = build_affordance_frame(prog, machine.state, args.projection, params)
    except Revert as exc:
        print(f"projection reverted: {exc.code}")
        return 1
    print(
        json.dumps(
            frame.to_json(),
            indent=2,
            sort_keys=True,
            default=lambda v: "0x" + v.hex() if isinstance(v, bytes) else str(v),
        )
    )
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
    p.add_argument("--timeout", type=int, default=20000)
    p.add_argument(
        "--unverified",
        action="store_true",
        help="execute without proof, for development only",
    )
    p.set_defaults(func=cmd_run)

    p = sub.add_parser("codegen", help="emit a compilation target")
    p.add_argument("target", choices=["rust", "cuda", "wgsl", "glsl"])
    p.add_argument("file")
    p.add_argument("--out")
    p.add_argument("--timeout", type=int, default=20000)
    p.add_argument(
        "--unverified",
        action="store_true",
        help="emit unproved code for development and fuzzing",
    )
    p.add_argument(
        "--no-main",
        action="store_true",
        help="Rust: emit a library without the demo entry point",
    )
    p.add_argument(
        "--runtime-crate",
        action="store_true",
        help="Rust: link the io_uring runtime crate",
    )
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
    from .runtime import Revert

    try:
        if hasattr(args, "timeout") and args.timeout <= 0:
            raise ValueError("timeout must be positive")
        return args.func(args)
    except VeraError as exc:
        print(exc.diag.render(), file=sys.stderr)
    except Revert as exc:
        print(f"REVERT({exc.code})", file=sys.stderr)
    except (ValueError, OSError, KeyError) as exc:
        print(f"error: {exc}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
