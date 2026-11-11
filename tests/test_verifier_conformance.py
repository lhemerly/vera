"""Behavioral regressions for the verifier's language semantics."""

import pytest
from vera.parser import parse_source
from vera.semantic import analyze
from vera.smt_emit import SmtConfig, SmtEncoder, verify_transition_dual


def program(source):
    result = analyze(parse_source(source))
    assert not result.diags.has_errors(), result.diags.render()
    return result


def checked(source, name="go", **config):
    return verify_transition_dual(
        program(source), name, SmtConfig(timeout_ms=10000, **config)
    )


HEADER = """module M;
table T capacity 2 { key id:id64<T>; col v:i64; }
"""


def test_admitted_large_fold_is_checked_instead_of_replaced_with_zero():
    result = checked(
        HEADER
        + """
    invariant zero { forall k in T : eq(T[k].v,0); }
    transition go(k:id64<T>) reads [] writes [T[k].v]
    require live:table_exists(T,k);
    body {
      let %v:i64=fold_bounded<257>(acc:i64=0,i in [0..256]) { yield 1; };
      table_set(T,k,v,%v); commit;
    }"""
    )
    assert not result.ok
    assert result.safety.status == "sat"


def test_lower_configured_unroll_limit_fails_closed():
    result = checked(
        HEADER
        + """
    transition go() reads [] writes [] body {
      let %v:i64=fold_bounded<8>(acc:i64=0,i in [0..7]) { yield 1; };
      commit;
    }""",
        max_unroll=4,
    )
    assert not result.ok
    assert any(d.severity == "error" for d in result.diagnostics)
    assert "VERDICT: VERIFIED" not in result.summary()


def test_allocation_does_not_hide_earlier_panic_in_a_full_table():
    result = checked(
        HEADER
        + """
    transition go(a:id64<T>,b:id64<T>) reads [] writes [T[*].v]
    body {
      let %den:i64=select(and(table_exists(T,a),and(table_exists(T,b),ne(a,b))),0,1);
      let %q:i64=div(1,%den);
      let %k:id64<T> =table_alloc(T,{v:0}); commit;
    }"""
    )
    assert not result.ok
    assert result.panic.status == "sat"


def test_lazy_select_does_not_evaluate_unchosen_arithmetic():
    result = checked(
        HEADER
        + """
    transition go() reads [] writes [] body {
      let %v:i64=select(true,1,div(1,0)); commit;
    }"""
    )
    assert result.ok, result.summary()


def test_boolean_operators_eagerly_evaluate_both_operands():
    result = checked(
        HEADER
        + """
    transition go() reads [] writes [] body {
      let %v:bool=and(false,eq(div(1,0),0)); commit;
    }"""
    )
    assert result.panic.status == "sat"


def test_inactive_fold_iterations_do_not_panic():
    result = checked(
        HEADER
        + """
    transition go() reads [] writes [] body {
      let %v:i64=fold_bounded<4>(acc:i64=0,i in [0..0]) {
        yield div(1,sub(1,i));
      }; commit;
    }"""
    )
    assert result.ok, result.summary()


def test_fold_index_does_not_wrap_at_integer_maximum():
    result = checked(
        HEADER
        + """
    transition go(k:id64<T>) reads [] writes [T[k].v]
    require live:table_exists(T,k);
    ensure one:eq(T[k].v,1);
    body {
      let %v:i64=fold_bounded<2>(acc:i64=0,i in [9223372036854775807..9223372036854775807]) {
        yield add(acc,1);
      };
      table_set(T,k,v,%v); commit;
    }"""
    )
    assert result.ok, result.summary()


def test_require_arithmetic_is_checked_before_body():
    result = checked(
        HEADER
        + """
    transition go(divisor:i64) reads [] writes []
    require quotient:eq(div(1,divisor),1);
    body { commit; }"""
    )
    assert result.panic.status == "sat"


def test_failed_require_skips_later_require_traps():
    result = checked(
        HEADER
        + """
    transition go(allowed:bool) reads [] writes []
    require allowed:allowed;
    require quotient:eq(select(allowed,1,div(1,0)),1);
    body { commit; }"""
    )
    assert result.ok, result.summary()


def test_capacity_one_valid_key_get_has_consistent_slot_and_generation():
    result = checked("""module One;
    table T capacity 1 {key id:id64<T>; col v:i64;}
    transition go(k:id64<T>) reads [T[k].v] writes []
    require live:table_exists(T,k);
    body {let %v:i64=table_get(T,k,v);commit;}""")
    assert result.ok, result.summary()


def test_allocated_row_quantifier_uses_new_generation():
    result = checked(
        HEADER
        + """
    invariant live {forall k in T:table_exists(T,k);}
    transition go() reads [] writes [T[*].v]
    body {let %k:id64<T> =table_alloc(T,{v:0});commit;}"""
    )
    assert result.ok, result.summary()


def test_non_scalar_aggregate_subexpression_widths_are_exact():
    result = checked("""module Aggregate;
    table T capacity 2 {key id:id64<T>;col v:u64 [tracked_sum];}
    transition go() reads [] writes []
    ensure identity:eq(mul(sum(T.v),0),0);
    body {commit;}""")
    assert result.ok, result.summary()


def test_enum_parameters_cannot_make_nonvacuity_using_invalid_tags():
    result = checked(
        HEADER
        + """
    transition go(tag:enum{A,B}) reads [] writes [] body {
      guard(ne(tag,A),ERR_A); guard(ne(tag,B),ERR_B); commit;
    }"""
    )
    assert result.liveness.status == "unsat"
    assert not result.ok


def test_full_precision_dec128_literals_are_encoded_as_scaled_integers():
    result = checked(
        HEADER
        + """
    transition go() reads [] writes [] body {
      let %tiny:dec128=0.000000000000000001;
      guard(eq(%tiny,1),ERR_PRECISION); commit;
    }"""
    )
    assert result.ok, result.summary()


def test_bytes32_literals_have_their_declared_bit_width():
    result = checked("""module Bytes;
    table T capacity 2 {key id:id64<T>;col hash:bytes32;}
    transition go(k:id64<T>) reads [] writes [T[k].hash]
    require live:table_exists(T,k);
    ensure hash:eq(T[k].hash,0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff);
    body {table_set(T,k,hash,0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff);commit;}""")
    assert result.ok, result.summary()


def test_nested_boolean_predicates_containing_sums_are_boolean_values():
    result = checked("""module Boolean;
    table T capacity 2 {key id:id64<T>;col v:u64 [tracked_sum];}
    transition go() reads [] writes []
    ensure truth:eq(ge(sum(T.v),0),true);
    body {commit;}""")
    assert result.ok, result.summary()


def test_aggregate_to_scalar_binding_checks_narrowing_domain():
    result = checked("""module Narrow;
    table T capacity 2 {key id:id64<T>;col v:u8 [tracked_sum];}
    transition go() reads [T[*].v] writes []
    body {let %v:u8=sum(T.v);commit;}""")
    assert result.panic.status == "sat"
    assert not result.ok


def test_newly_allocated_unique_key_is_found_in_same_transaction():
    result = checked("""module Unique;
    table T capacity 2 {key id:id64<T>;col code:u64 [unique];}
    transition go(value:u64) reads [T[*].code] writes [T[*].code]
    body {
      let %new:id64<T> =table_alloc(T,{code:value});
      let %found:id64<T> =table_lookup(T,code,value);
      guard(eq(%new,%found),ERR_LOOKUP);
      commit;
    }""")
    assert result.ok, result.summary()


def test_constructor_from_small_integer_has_64_bit_handle_width():
    result = checked(
        HEADER
        + """
    transition go(value:u8) reads [] writes []
    body {let %exists:bool=table_exists(T,id64<T>(value));commit;}"""
    )
    assert result.ok, result.summary()


def test_stale_and_live_handles_alias_by_slot_in_sum_frame():
    result = checked("""module Alias;
    table T capacity 2 {key id:id64<T>;col v:i64 [tracked_sum];}
    invariant total {eq(sum(T.v),100);}
    invariant nonnegative {forall k in T:ge(T[k].v,0);}
    invariant bounded {forall k in T:le(T[k].v,100);}
    transition go(k:id64<T>,stale:id64<T>) reads [T[k].v] writes [T[k].v]
    require fixed:eq(k,id64<T>(2));
    require other_generation:eq(stale,id64<T>(4));
    require live:table_exists(T,k);
    require stale:not(table_exists(T,stale));
    require zero:eq(T[k].v,0);
    body {table_set(T,k,v,1);commit;}""")
    assert result.safety.status == "sat"
    assert not result.ok


def test_cross_table_invariant_checks_untouched_referencing_rows():
    result = checked("""module Foreign;
    table A capacity 2 {key id:id64<A>;col v:i64;}
    table B capacity 2 {key id:id64<B>;col owner:id64<A>;}
    invariant balance {forall k in B:ge(A[B[k].owner].v,0);}
    transition go() reads [] writes [A[*].v]
    body {let %new:id64<A> =table_alloc(A,{v:-1});commit;}""")
    assert result.safety.status == "sat"
    assert not result.ok


def test_nonvacuity_requires_a_realizable_exact_sum_state():
    result = checked("""module Parity;
    table T capacity 2 {key id:id64<T>;col v:i64 [tracked_sum];}
    invariant even {forall k in T:eq(rem(T[k].v,2),0);}
    invariant odd_total {eq(sum(T.v),1);}
    transition go() reads [] writes [] body {commit;}""")
    assert result.liveness.status == "unsat"
    assert not result.ok


def test_preinvariant_pure_fold_reads_prestate():
    result = checked(
        HEADER
        + """
    invariant zero {eq(fold_bounded<1>(acc:i64=0,i in [0..0]) {
      yield table_get(T,id64<T>(2),v);
    },0);}
    transition go(k:id64<T>) reads [] writes [T[k].v]
    require fixed:eq(k,id64<T>(2));
    require live:table_exists(T,k);
    body {table_set(T,k,v,1);commit;}"""
    )
    assert result.safety.status == "sat"
    assert not result.ok


def test_inactive_fold_allocation_preserves_live_generation():
    result = checked("""module Inactive;
    table T capacity 1 {key id:id64<T>;col v:i64;}
    invariant live {forall k in T:table_exists(T,k);}
    transition go() reads [] writes [T[*].v]
    body {
      let %n:i64=fold_bounded<2>(acc:i64=0,i in [0..0]) {
        let %new:id64<T> =table_alloc(T,{v:1});
        yield 1;
      };
      commit;
    }""")
    assert result.ok, result.summary()


def test_exported_dual_proof_script_has_no_duplicate_declarations():
    import z3

    result = checked(HEADER + "transition go() reads [] writes [] body {commit;}")
    assert result.ok, result.summary()
    z3.parse_smt2_string(result.script)
    assert result.script.count("(check-sat)") == 3
