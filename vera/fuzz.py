"""Reference implementation of the GPU fuzzing furnace (Section 7).

Each simulated thread owns a compact sandbox state and a deterministic
xoshiro256** PRNG, runs a trajectory of D precondition-guided transitions, and
evaluates every global invariant after each step.  On violation, a parallel
delta-shrinking pass (simulated sequentially) minimizes the trace.

The CUDA/WGSL backends emit the same semantics for real GPUs; this module is
the executable reference used by the test-suite and by ``vera fuzz``.
"""

from dataclasses import dataclass, field
from typing import Dict, List, Optional, Tuple

from . import ast_nodes as A
from .errors import ERR_VACUOUS_IMPLEMENTATION
from .runtime import Machine, Revert, State, Transaction
from .semantic import Program
from .types import ScalarType, id64


MASK64 = (1 << 64) - 1


class Xoshiro256:
    """xoshiro256** PRNG (deterministic, seedable)."""

    def __init__(self, seed: int):
        self.s = [0, 0, 0, 0]
        x = seed & MASK64
        for i in range(4):
            x = (x + 0x9E3779B97F4A7C15) & MASK64
            z = x
            z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & MASK64
            z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & MASK64
            self.s[i] = (z ^ (z >> 31)) & MASK64

    @staticmethod
    def _rotl(x: int, k: int) -> int:
        return ((x << k) | (x >> (64 - k))) & MASK64

    def next_u64(self) -> int:
        s = self.s
        result = (self._rotl((s[1] * 5) & MASK64, 7) * 9) & MASK64
        t = (s[1] << 17) & MASK64
        s[2] ^= s[0]
        s[3] ^= s[1]
        s[1] ^= s[2]
        s[0] ^= s[3]
        s[2] ^= t
        s[3] = self._rotl(s[3], 45)
        return result

    def below(self, n: int) -> int:
        return self.next_u64() % n if n > 0 else 0


@dataclass
class Violation:
    thread: int
    step: int
    law: str
    detail: str
    trace: List[Tuple[str, Dict[str, object]]] = field(default_factory=list)


@dataclass
class FuzzResult:
    ok: bool
    steps: int
    threads: int
    violations: List[Violation] = field(default_factory=list)
    executed: int = 0
    committed: int = 0

    def render(self) -> str:
        lines = [
            f"VERA reference furnace: {self.threads} threads x {self.steps} steps",
            f"  transitions executed: {self.executed}",
            f"  committed: {self.committed}",
            f"  violations: {len(self.violations)}",
        ]
        for v in self.violations[:5]:
            lines.append(f"  - thread {v.thread} step {v.step}: {v.law} ({v.detail})")
            for name, params in v.trace[-4:]:
                lines.append(f"      {name}({params})")
        if not self.violations:
            lines.append("  RESULT: NO VIOLATIONS FOUND")
        else:
            lines.append("  RESULT: VIOLATIONS FOUND")
        return "\n".join(lines)


def _eval_invariants(
    prog: Program,
    state: State,
    pre_agg: Dict[str, Dict[str, int]],
    pre_state: Optional[State] = None,
) -> List[str]:
    out: List[str] = []
    for inv in prog.invariants.values():
        if inv.quant_table is None:
            try:
                tx = Transaction(prog, state, "_fuzz", {})
                tx.pre_agg = pre_agg
                if pre_state is not None:
                    tx.pre_state = pre_state
                val = tx.value_of(inv.expr, "post")
            except Revert:
                val = False
            if val is not True:
                out.append(inv.name)
            continue
        t = state.tables[inv.quant_table]
        for slot in range(t.capacity):
            if not t.occ[slot]:
                continue
            key = (t.gen[slot] << t.key_bits) | slot
            try:
                tx = Transaction(prog, state, "_fuzz", {})
                tx.pre_agg = pre_agg
                if pre_state is not None:
                    tx.pre_state = pre_state
                tx.env[inv.quant_var] = key
                tx.type_env[inv.quant_var] = id64(inv.quant_table)
                val = tx.value_of(inv.expr, "post")
            except Revert:
                val = False
            if val is not True:
                out.append(inv.name)
                break
    return out


def _sample_params(
    prog: Program, ti, rng: Xoshiro256, state: State
) -> Dict[str, object]:
    params: Dict[str, object] = {}
    for p in ti.decl.params:
        ctype = ti.param_types[p.name]
        if ctype.kind == "id64":
            t = state.tables[ctype.key_table]
            live = [i for i in range(t.capacity) if t.occ[i]]
            if live:
                slot = live[rng.below(len(live))]
                params[p.name] = (t.gen[slot] << t.key_bits) | slot
            else:
                params[p.name] = rng.below(t.capacity)
        elif ctype.is_decimal:
            params[p.name] = rng.below(2_000_000_000)
        elif ctype.is_bool:
            params[p.name] = rng.below(2) == 1
        elif ctype.kind == "enum":
            params[p.name] = rng.below(len(ctype.enum_tags))
        elif ctype.kind == "bytes32":
            params[p.name] = bytes(rng.below(256) for _ in range(32))
        else:
            params[p.name] = rng.below(max(2, ctype.max_value))
    return params


def _seed_state(
    prog: Program, machine: Machine, rng: Xoshiro256, seed_limit: Optional[int] = None
) -> None:
    """Seed a few live rows so precondition-guided sampling has material."""
    for tname, tinfo in prog.tables.items():
        t = machine.state.tables[tname]
        count = min(4, tinfo.layout.capacity // 4)
        if seed_limit is not None:
            count = min(count, seed_limit)
        for slot in range(count):
            t.occ[slot] = 1
            t.gen[slot] = 1
            for cname, cinfo in tinfo.columns.items():
                if cinfo.type.kind == "id64":
                    ref = prog.tables[cinfo.type.key_table]
                    t.columns[cname][slot] = (1 << ref.layout.key_bits) | 0
                elif cinfo.type.is_decimal:
                    t.columns[cname][slot] = rng.below(900_000_000)
                elif cinfo.type.is_bool:
                    t.columns[cname][slot] = False
                elif cinfo.type.kind == "enum":
                    t.columns[cname][slot] = 0
                elif cinfo.type.kind == "bytes32":
                    t.columns[cname][slot] = (slot + 1 if cinfo.unique else 0).to_bytes(
                        32, "little"
                    )
                else:
                    t.columns[cname][slot] = slot + 1 if cinfo.unique else 0
        machine.recompute_aggregates()
    machine.rebuild_indexes()


def fuzz_module(
    prog: Program,
    steps: int = 64,
    threads: int = 256,
    seed: int = 12345,
    capacity: Optional[int] = None,
) -> FuzzResult:
    if steps < 0 or threads < 0:
        raise ValueError("fuzzer steps and threads must be nonnegative")
    if capacity is not None and (type(capacity) is not int or capacity < 0):
        raise ValueError("seed row limit must be a nonnegative integer")
    result = FuzzResult(ok=True, steps=steps, threads=threads)
    transitions = list(prog.transitions)
    if not transitions:
        return result
    for tid in range(threads):
        rng = Xoshiro256((seed * 1_000_003 + tid) & MASK64)
        machine = Machine(prog)
        try:
            _seed_state(prog, machine, rng, seed_limit=capacity)
            machine.validate_state()
        except Revert:
            # Sampling must never report an invariant already false before a
            # transition. Empty tables are the canonical alternative seed.
            machine = Machine(prog)
            try:
                machine.validate_state()
            except Revert as exc:
                raise ValueError(
                    "fuzzer cannot construct a valid initial state"
                ) from exc
        initial_state = machine.state.clone()
        trace: List[Tuple[str, Dict[str, object]]] = []
        for step in range(steps):
            tname = transitions[rng.below(len(transitions))]
            ti = prog.transitions[tname]
            params = _sample_params(prog, ti, rng, machine.state)
            pre_agg = {t: dict(x.agg) for t, x in machine.state.tables.items()}
            violated = []
            try:
                machine.execute(tname, params, record_trace=False)
                result.executed += 1
                result.committed += 1
            except Revert as exc:
                if exc.kind == "precondition":
                    continue
                result.executed += 1
                trace.append((tname, params))
                if exc.kind != "trap":
                    continue
                violated = ["panic freedom"]
            else:
                trace.append((tname, params))
                violated = _eval_invariants(
                    prog, machine.state, pre_agg, machine.last_tx.pre_state
                )
                if not violated:
                    violated = _eval_postconditions(
                        prog, machine, tname, params, pre_agg
                    )
            if violated:
                result.ok = False
                for law in violated:
                    result.violations.append(
                        Violation(
                            thread=tid,
                            step=step,
                            law=law,
                            detail="unhandled execution trap"
                            if law == "panic freedom"
                            else "postcondition/invariant violated",
                            trace=_shrink_trace(prog, initial_state, trace, law),
                        )
                    )
                break
    return result


def _eval_postconditions(
    prog: Program,
    machine: Machine,
    tname: str,
    params: Dict[str, object],
    pre_agg: Dict[str, Dict[str, int]],
) -> List[str]:
    out: List[str] = []
    ti = prog.transitions[tname]
    if not ti.decl.ensures:
        return out
    last = getattr(machine, "last_tx", None)
    final_env = dict(last.env) if last is not None else {}
    for name, expr in ti.decl.ensures:
        try:
            tx = Transaction(prog, machine.state, "_fuzz_post", params)
            tx.env = dict(final_env)
            if last is not None:
                tx.type_env = dict(last.type_env)
                tx.pre_state = last.pre_state
            tx.pre_agg = pre_agg
            val = tx.value_of(expr, "post")
        except Revert:
            val = False
        if val is not True:
            out.append(f"ensure {name}")
    return out


def _trace_violates(
    prog: Program, initial: State, trace: List[Tuple[str, Dict[str, object]]], law: str
) -> bool:
    machine = Machine(prog, initial.clone())
    for name, params in trace:
        before = {
            table: dict(state.agg) for table, state in machine.state.tables.items()
        }
        try:
            machine.call(name, params)
        except Revert as exc:
            if law == "panic freedom" and exc.kind == "trap":
                return True
            continue
        laws = _eval_invariants(prog, machine.state, before, machine.last_tx.pre_state)
        laws += _eval_postconditions(prog, machine, name, params, before)
        if law in laws:
            return True
    return False


def _shrink_trace(
    prog: Program, initial: State, trace: List[Tuple[str, Dict[str, object]]], law: str
) -> List[Tuple[str, Dict[str, object]]]:
    """Deterministic delta reduction followed by numeric argument shrinking."""
    best = [(name, dict(params)) for name, params in trace]
    chunk = max(1, len(best) // 2)
    while chunk:
        changed = False
        for start in range(0, len(best), chunk):
            candidate = best[:start] + best[start + chunk :]
            if candidate and _trace_violates(prog, initial, candidate, law):
                best = candidate
                changed = True
                break
        if not changed:
            chunk //= 2
    for index, (name, params) in enumerate(list(best)):
        for param, value in list(params.items()):
            ctype = prog.transitions[name].param_types[param]
            if type(value) is not int or ctype.kind in ("id64", "sym32", "enum"):
                continue
            sign = -1 if value < 0 else 1
            lower, upper = 0, abs(value)
            while lower < upper:
                mid = (lower + upper) // 2
                proposed = sign * mid
                candidate = [(n, dict(p)) for n, p in best]
                candidate[index][1][param] = proposed
                if _trace_violates(prog, initial, candidate, law):
                    best = candidate
                    upper = mid
                else:
                    lower = mid + 1
    return best
