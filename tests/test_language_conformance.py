"""Observable language semantics shared by the interpreter and code generators."""

from decimal import Decimal

import pytest

from vera.parser import parse_source
from vera.semantic import analyze
from vera.runtime import Machine, Revert
from vera.errors import VeraError


def program(source):
    p = analyze(parse_source(source))
    assert not p.diags.has_errors(), p.diags.render()
    return p


def test_failed_unique_allocation_restores_indexes():
    p = program("""module M;
    table T capacity 2 {key id:id64<T>; col code:u64 [unique];}
    transition make(value:u64,accept:bool) reads [] writes [T[*].code]
    body {let %k:id64<T> = table_alloc(T,{code:value});guard(accept,ERR_REJECT);commit;}
    """)
    m = Machine(p)
    before = m.state.clone()
    with pytest.raises(Revert, match="ERR_REJECT"):
        m.call("make", {"value": 1, "accept": False})
    assert m.state == before
    m.call("make", {"value": 1, "accept": True})
    assert sum(m.state.tables["T"].hash_used["code"]) == 1


def test_provided_state_is_preserved():
    p = program("""module M; table T capacity 2 {key id:id64<T>;col v:u64;}
    transition put(k:id64<T>,v:u64) reads [] writes [T[k].v]
    require live:table_exists(T,k);body {table_set(T,k,v,v);commit;}""")
    m = Machine(p)
    t = m.state.tables["T"]
    t.occ[0] = 1
    t.gen[0] = 1
    t.columns["v"][0] = 91
    restored = Machine(p, m.state.clone())
    assert restored.state.tables["T"].columns["v"][0] == 91


def test_exact_dec128_literal_and_decimal_multiply():
    p = program("""module M;table T capacity 2 {key id:id64<T>;col v:dec128;}
    transition make() reads [] writes [T[*].v]
    body {let %x:dec128=mul(1.000000000000000001,2.0);
    let %k:id64<T>=table_alloc(T,{v:%x});commit;}""")
    m = Machine(p)
    m.call("make", {})
    assert m.state.tables["T"].columns["v"][0] == 2000000000000000002


@pytest.mark.parametrize("location", ["body", "require"])
def test_negative_dec128_literals_are_not_rounded_by_host_decimal_context(location):
    exact = -10000000000000000000000000001
    source_literal = "-10000000000.000000000000000001"
    requires = (
        f"require exact:eq(value,{source_literal});" if location == "require" else ""
    )
    value = source_literal if location == "body" else "value"
    p = program(f"""module Exact;
    table T capacity 2 {{key id:id64<T>;col v:dec128;}}
    transition create(value:dec128) reads [] writes [T[*].v]
    {requires}
    body {{let %k:id64<T> =table_alloc(T,{{v:{value}}});commit;}}""")
    machine = Machine(p)
    machine.call("create", {"value": exact})
    assert machine.state.tables["T"].columns["v"][0] == exact


def test_u8_context_checks_literal_only_arithmetic():
    p = program("""module M;table T capacity 2 {key id:id64<T>;col v:u8;}
    transition make() reads [] writes [T[*].v]
    body {let %x:u8=add(250,10);let %k:id64<T>=table_alloc(T,{v:%x});commit;}""")
    with pytest.raises(Revert, match="ERR_ARITH_OVERFLOW"):
        Machine(p).call("make", {})


def test_old_and_old_sum_read_transaction_start():
    p = program("""module M;table T capacity 2 {key id:id64<T>;col v:i64 [tracked_sum];}
    transition update(k:id64<T>) reads [T[*].v] writes [T[k].v]
    require live:table_exists(T,k);
    body {table_set(T,k,v,9);let %old:i64=old(T[k].v);
    guard(eq(%old,7),ERR_OLD);guard(eq(old_sum(T.v),7),ERR_OLD_SUM);commit;}""")
    m = Machine(p)
    t = m.state.tables["T"]
    t.occ[0] = 1
    t.gen[0] = 1
    t.columns["v"][0] = 7
    m.recompute_aggregates()
    m.call("update", {"k": 2})
    assert t.columns["v"][0] == 9


def test_parameter_domains_checked_before_execution():
    p = program("""module M;table T capacity 2 {key id:id64<T>;col v:u8;}
    transition make(v:u8) reads [] writes [T[*].v]
    body {let %k:id64<T>=table_alloc(T,{v:v});commit;}""")
    m = Machine(p)
    for params in ({"v": True}, {"v": 256}, {"v": -1}, {"v": 1, "unused": 2}, {}):
        with pytest.raises(Revert):
            m.call("make", params)
        assert not any(m.state.tables["T"].occ)


def test_ssa_redefinitions_and_missing_commit_rejected():
    for body in (
        "let %x:u64=1;let %x:u64=2;commit;",
        "let %x:u64=1;",
        "commit;let %x:u64=1;",
    ):
        try:
            p = analyze(
                parse_source(
                    "module M;table T capacity 2 {key id:id64<T>;col v:u64;}transition t() reads [] writes [] body {"
                    + body
                    + "}"
                )
            )
        except VeraError:
            continue
        assert p.diags.has_errors()


def test_self_and_forward_foreign_keys_are_resolved():
    p = program("""module M;table A capacity 2 {key id:id64<A>;col parent:id64<A>;col b:id64<B>;}
    table B capacity 2 {key id:id64<B>;col v:u64;}
    transition t() reads [] writes [] body {commit;}""")
    assert p.tables["A"].columns["b"].type.key_table == "B"


def test_generation_uses_physical_u32_domain():
    p = program("""module M;table T capacity 2 {key id:id64<T>;col v:u64;}
    transition make() reads [] writes [T[*].v]
    body {let %k:id64<T>=table_alloc(T,{v:0});commit;}""")
    m = Machine(p)
    m.state.tables["T"].gen[0] = 2**32 - 1
    with pytest.raises(Revert, match="ERR_GENERATION_EXHAUSTED"):
        m.call("make", {})


def test_invalid_fractional_precision_rejected():
    p = analyze(
        parse_source("""module M;table T capacity 2 {key id:id64<T>;col v:dec64;}
    transition make() reads [] writes [T[*].v]
    body {let %k:id64<T>=table_alloc(T,{v:0.000000001});commit;}""")
    )
    assert p.diags.has_errors()


def test_nested_folds_keep_lexical_accumulator_types():
    p = program("""module M;table T capacity 2 {key id:id64<T>;col v:u64;}
    transition make(n:u8) reads [] writes [T[*].v]
    body {let %v:u64=fold_bounded<3>(acc:u64=0,i in [0..n]) {
      let %inner:u64=fold_bounded<2>(inside:u64=0,j in [0..1]) {yield add(inside,1);};
      yield add(acc,%inner);
    };let %k:id64<T>=table_alloc(T,{v:%v});commit;}""")
    m = Machine(p)
    m.call("make", {"n": 2})
    assert m.state.tables["T"].columns["v"][0] == 6


def test_bytes32_symbols_enum_tags_and_effect_outbox():
    from vera.types import symbol_id

    p = program("""module M;
    table T capacity 2 {key id:id64<T>;col digest:bytes32;col state:enum{Open,Closed};col symbol:sym32;}
    effect Created(digest:bytes32,state:enum{Open,Closed},symbol:sym32);
    transition make() reads [] writes [T[*].digest,T[*].state,T[*].symbol]
    body {let %k:id64<T>=table_alloc(T,{digest:0x1234,state:Closed,symbol:"alpha"});
      emit Created(0x1234,Closed,"alpha");commit;}""")
    m = Machine(p)
    m.call("make", {})
    assert m.state.tables["T"].columns["digest"][0] == bytes.fromhex("3412" + "00" * 30)
    assert m.state.tables["T"].columns["state"][0] == 1
    assert m.state.outbox == [
        ("Created", [bytes.fromhex("3412" + "00" * 30), 1, symbol_id("alpha")])
    ]


def test_short_circuit_select_and_eager_boolean_operators():
    p = program("""module M;table T capacity 2 {key id:id64<T>;col v:i64;}
    transition make() reads [] writes [T[*].v]
    body {let %v:i64=select(true,3,div(1,0));let %k:id64<T>=table_alloc(T,{v:%v});commit;}
    transition eager() reads [] writes []
    body {guard(or(true,eq(div(1,0),0)),ERR_REJECT);commit;}""")
    m = Machine(p)
    m.call("make", {})
    assert m.state.tables["T"].columns["v"][0] == 3
    with pytest.raises(Revert, match="ERR_ARITH_OVERFLOW"):
        m.call("eager", {})


def test_robin_hood_displacement_and_rollback_are_observable():
    from vera.types import bucket_of

    # Insert a resident in bucket 1, then two entries starting at bucket 0;
    # the last incoming entry displaces the nearer resident at bucket 1.
    values = {0: [], 1: []}
    for candidate in range(1000):
        bucket = bucket_of(candidate, 16)
        if bucket in values and len(values[bucket]) < 2:
            values[bucket].append(candidate)
    a, b = values[0]
    c = values[1][0]
    p = program("""module M;table T capacity 4 {key id:id64<T>;col code:u64 [unique];}
    transition make(value:u64,accept:bool) reads [] writes [T[*].code]
    body {let %k:id64<T>=table_alloc(T,{code:value});guard(accept,ERR_REJECT);commit;}
    transition find(value:u64) reads [T[*].code] writes []
    body {let %k:id64<T>=table_lookup(T,code,value);commit;}""")
    m = Machine(p)
    for v in (c, a):
        m.call("make", {"value": v, "accept": True})
    before = m.state.clone()
    with pytest.raises(Revert):
        m.call("make", {"value": b, "accept": False})
    assert m.state == before
    m.call("make", {"value": b, "accept": True})
    for v in (a, b, c):
        m.call("find", {"value": v})
    m.validate_state()


def test_external_state_admission_checks_invariants_and_derived_data():
    p = program("""module M;table T capacity 2 {key id:id64<T>;col v:i64 [tracked_sum];}
    invariant nonnegative {forall k in T:ge(T[k].v,0);}
    transition t() reads [] writes [] body {commit;}""")
    m = Machine(p)
    t = m.state.tables["T"]
    t.occ[0] = 1
    t.gen[0] = 1
    t.columns["v"][0] = 2
    with pytest.raises(Revert, match="ERR_INVALID_STATE"):
        m.validate_state()
    m.recompute_aggregates()
    m.validate_state()
    t.columns["v"][0] = -1
    m.recompute_aggregates()
    with pytest.raises(Revert, match="ERR_INVARIANT_nonnegative"):
        m.validate_state()


def test_projection_does_not_expose_a_stale_handle():
    from vera.projection import build_affordance_frame

    p = program("""module M;table T capacity 2 {key id:id64<T>;col v:u64;}
    projection View(k:id64<T>) {allow_if:true;expose:[T[k].v];enable_intents:[];}
    transition t() reads [] writes [] body {commit;}""")
    m = Machine(p)
    t = m.state.tables["T"]
    t.occ[0] = 1
    t.gen[0] = 1
    t.columns["v"][0] = 42
    frame = build_affordance_frame(p, m.state, "View", {"k": 4})
    assert frame.state_slice["T.v[4]"]["live"] is False
    assert frame.state_slice["T.v[4]"]["value"] is None


def test_fuzzer_returns_a_replayable_minimized_trajectory():
    from vera.fuzz import fuzz_module

    p = program("""module M;table T capacity 8 {key id:id64<T>;col v:u64;}
    invariant zero {forall k in T:eq(T[k].v,0);}
    transition noop() reads [] writes [] body {commit;}
    transition broken(v:u64) reads [] writes [T[*].v]
    body {let %k:id64<T>=table_alloc(T,{v:v});commit;}""")
    result = fuzz_module(p, steps=8, threads=1, seed=7)
    assert not result.ok
    trace = result.violations[0].trace
    assert trace == [("broken", {"v": 1})]


def test_pure_fold_resource_limits_and_state_mutation_are_rejected_in_specs():
    for expr in (
        "fold_bounded<1025>(a:i64=0,i in [0..1]) {yield 0;}",
        "fold_bounded<2>(a:i64=0,i in [0..1]) {let %k:id64<T>=table_alloc(T,{v:0});yield 0;}",
    ):
        p = analyze(
            parse_source(
                "module M;table T capacity 2 {key id:id64<T>;col v:i64;}invariant bad {eq("
                + expr
                + ",0);}transition t() reads [] writes [] body {commit;}"
            )
        )
        assert p.diags.has_errors()


def test_literal_context_is_inferred_from_either_typed_operand():
    p = program("""module M;table T capacity 2 {key id:id64<T>;col v:dec128;col small:u8;}
    transition make(x:dec128,n:u8) reads [] writes [T[*].v,T[*].small]
    require exact:eq(mul(1.000000000000000001,x),add(x,2));
    body {let %v:dec128=mul(1.000000000000000001,x);
    let %small:u8=select(true,0,n);
    let %k:id64<T>=table_alloc(T,{v:%v,small:%small});commit;}""")
    m = Machine(p)
    m.call("make", {"x": 2000000000000000000, "n": 3})
    assert m.state.tables["T"].columns["v"][0] == 2000000000000000002
    assert m.state.tables["T"].columns["small"][0] == 0


def test_fuzzer_distinguishes_traps_from_handled_guard_reverts():
    from vera.fuzz import fuzz_module

    panic = program("""module M;table T capacity 2 {key id:id64<T>;col v:i64;}
    transition broken() reads [] writes []
    body {let %v:i64=div(1,0);commit;}""")
    result = fuzz_module(panic, steps=1, threads=1)
    assert not result.ok
    assert result.violations[0].law == "panic freedom"
    handled = program("""module M;table T capacity 2 {key id:id64<T>;col v:i64;}
    transition rejection() reads [] writes []
    body {guard(false,ERR_ARITH_OVERFLOW);commit;}""")
    assert fuzz_module(handled, steps=1, threads=1).ok
