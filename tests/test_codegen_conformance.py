"""Behavioral conformance of generated Rust: transactions, values, and replay."""

import shutil
import subprocess

import pytest

from vera.codegen_rust import generate_rust
from vera.parser import parse_source
from vera.semantic import analyze


def run_rust(tmp_path, source, main):
    if shutil.which("rustc") is None:
        pytest.skip("rustc unavailable")
    prog = analyze(parse_source(source))
    assert not prog.diags.has_errors(), prog.diags.render()
    rust = tmp_path / "case.rs"
    rust.write_text(generate_rust(prog, include_main=False) + "\n" + main)
    binary = tmp_path / "case"
    build = subprocess.run(
        ["rustc", "--edition", "2021", "-O", str(rust), "-o", str(binary)],
        capture_output=True,
        text=True,
    )
    assert build.returncode == 0, build.stderr
    result = subprocess.run([str(binary)], cwd=tmp_path, capture_output=True, text=True)
    assert result.returncode == 0, result.stderr
    return result.stdout


def test_rejected_write_restores_state(tmp_path):
    out = run_rust(
        tmp_path,
        """
module Atomic;
table T capacity 2 { key id: id64<T>; col v:u64; }
transition change(k:id64<T>, value:u64, accept:bool) reads [] writes [T[k].v]
require live: table_exists(T,k);
body { table_set(T,k,v,value); guard(accept,ERR_REJECT); commit; }
""",
        """fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;s.t.col_v[0]=7;assert!(change(&mut s,2,99,false).is_err());assert_eq!(s.t.col_v[0],7);println!("atomic");}""",
    )
    assert "atomic" in out


def test_widened_sums_do_not_wrap(tmp_path):
    out = run_rust(
        tmp_path,
        """
module Sums;
table T capacity 2 {key id:id64<T>;col v:i64 [tracked_sum];}
transition fill() reads [] writes [T[*].v]
body {let %k:id64<T> =table_alloc(T,{v:9223372036854775807});commit;}
""",
        """fn main(){let mut s=State::new();fill(&mut s).unwrap();fill(&mut s).unwrap();assert_eq!(s.t.agg_sum_v,18446744073709551614i128);println!("wide");}""",
    )
    assert "wide" in out


def test_batch_matches_input_order_and_replay(tmp_path):
    out = run_rust(
        tmp_path,
        """
module Batch;
table T capacity 2 {key id:id64<T>;col v:u64;}
transition put(k:id64<T>,value:u64) reads [] writes [T[k].v]
require live:table_exists(T,k); body {table_set(T,k,v,value);commit;}
transition copy(src:id64<T>,dst:id64<T>) reads [T[src].v] writes [T[dst].v]
require a:table_exists(T,src);require b:table_exists(T,dst);
body {let %v:u64=table_get(T,src,v);table_set(T,dst,v,%v);commit;}
""",
        """fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=3;s.t.generation[0]=1;s.t.generation[1]=1;let mut replay=s.clone();let intents=vec![Params::Put{k:2,value:1},Params::Copy{src:2,dst:3},Params::Put{k:3,value:2}];assert!(run_batch(&mut s,&intents).iter().all(Result::is_ok));for p in &intents {let (id,args)=p.encode();Params::decode(id,args).unwrap().execute(&mut replay).unwrap();}assert_eq!(s.t.col_v,[1,2]);assert_eq!(s,replay);println!("ordered");}""",
    )
    assert "ordered" in out


def test_all_parameter_bytes_survive_codec(tmp_path):
    out = run_rust(
        tmp_path,
        """
module Codec;
table T capacity 2 {key id:id64<T>;col v:bytes32;}
transition put(value:bytes32,flag:bool,a:u8,b:u16,c:u32,d:dec128) reads [] writes [T[*].v]
body {let %k:id64<T> =table_alloc(T,{v:value});commit;}
""",
        """fn main(){let p=Params::Put{value:[171;32],flag:true,a:255,b:65535,c:4294967295,d:i128::MIN};let (id,args)=p.encode();assert_eq!(Params::decode(id,&args).unwrap(),p);assert!(Params::decode(id,&args[..args.len()-1]).is_err());assert!(Params::decode(u32::MAX,args).is_err());println!("codec");}""",
    )
    assert "codec" in out


def test_effects_and_old_state_are_transactional(tmp_path):
    out = run_rust(
        tmp_path,
        """
module Effects;
table T capacity 2 {key id:id64<T>;col v:i64 [tracked_sum];}
effect Notice(before:i64,after:i64);
transition change(k:id64<T>,accept:bool) reads [T[k].v] writes [T[k].v]
require live:table_exists(T,k);
body {table_set(T,k,v,9);emit Notice(old(T[k].v),table_get(T,k,v));guard(accept,ERR_REJECT);commit;}
""",
        """fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;s.t.col_v[0]=7;s.t.agg_sum_v=7;assert!(change(&mut s,2,false).is_err());assert!(s.outbox.is_empty());change(&mut s,2,true).unwrap();assert_eq!(s.outbox,vec![Effect::Notice{before:7,after:9}]);println!("effects");}""",
    )
    assert "effects" in out


def test_checked_arithmetic_and_lazy_select(tmp_path):
    out = run_rust(
        tmp_path,
        """
module Arithmetic;
table T capacity 2 {key id:id64<T>;col v:u8;}
transition put(k:id64<T>,a:u8,b:u8) reads [] writes [T[k].v]
require live:table_exists(T,k);
body {let %x:u8=select(eq(b,0),min(a,9),max(add(a,b),7));table_set(T,k,v,%x);commit;}
""",
        """fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;put(&mut s,2,255,0).unwrap();assert_eq!(s.t.col_v[0],9);assert!(put(&mut s,2,255,1).is_err());assert_eq!(s.t.col_v[0],9);println!("checked");}""",
    )
    assert "checked" in out


def test_fold_index_does_not_wrap_at_type_max(tmp_path):
    out = run_rust(
        tmp_path,
        """
module Fold;
table T capacity 2 {key id:id64<T>;col v:i64;}
transition put(k:id64<T>,lo:u8,hi:u8) reads [] writes [T[k].v]
require live:table_exists(T,k);
body {let %x:i64=fold_bounded<4>(acc:i64=0,i in [lo..hi]) {yield add(acc,1);};table_set(T,k,v,%x);commit;}
""",
        """fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;put(&mut s,2,254,255).unwrap();assert_eq!(s.t.col_v[0],2);println!("fold");}""",
    )
    assert "fold" in out


def test_dec128_full_width_arithmetic_and_aggregate(tmp_path):
    out = run_rust(
        tmp_path,
        """
module WideDecimal;
table T capacity 2 {key id:id64<T>;col v:dec128 [tracked_sum];}
transition create(value:dec128) reads [] writes [T[*].v]
body {let %k:id64<T> =table_alloc(T,{v:value});commit;}
transition product(k:id64<T>,a:dec128,b:dec128) reads [] writes [T[k].v]
require live:table_exists(T,k);
body {table_set(T,k,v,mul(a,b));commit;}
transition quotient(k:id64<T>,a:dec128,b:dec128) reads [] writes [T[k].v]
require live:table_exists(T,k);
body {table_set(T,k,v,div(a,b));commit;}
""",
        """fn main(){let mut s=State::new();create(&mut s,i128::MAX).unwrap();create(&mut s,i128::MAX).unwrap();assert_eq!(format!("{}",s.t.agg_sum_v),"340282366920938463463374607431768211454");product(&mut s,2,i128::MAX,1000000000000000000).unwrap();assert_eq!(s.t.col_v[0],i128::MAX);quotient(&mut s,2,i128::MIN,1000000000000000000).unwrap();assert_eq!(s.t.col_v[0],i128::MIN);assert!(quotient(&mut s,2,i128::MIN,-1000000000000000000).is_err());assert_eq!(s.t.col_v[0],i128::MIN);assert_eq!(State::decode(s.encode()).unwrap(),s);println!("decimal128");}""",
    )
    assert "decimal128" in out


def test_unique_index_and_snapshot_restore(tmp_path):
    out = run_rust(
        tmp_path,
        """
module UniqueSnapshot;
table T capacity 4 {key id:id64<T>;col code:bytes32 [unique];col v:i64 [tracked_sum];}
effect Created(code:bytes32);
transition create(code:bytes32,value:i64,accept:bool) reads [] writes [T[*].code,T[*].v]
body {let %k:id64<T> =table_alloc(T,{code:code,v:value});emit Created(code);guard(accept,ERR_REJECT);commit;}
transition find(code:bytes32) reads [T[*].code,T[*].v] writes []
body {let %k:id64<T> =table_lookup(T,code,code);let %v:i64=table_get(T,%k,v);guard(eq(%v,7),ERR_VALUE);commit;}
""",
        """fn main(){let mut s=State::new();assert!(create(&mut s,[1;32],7,false).is_err());assert_eq!(s,State::new());create(&mut s,[1;32],7,true).unwrap();find(&mut s,[1;32]).unwrap();assert!(create(&mut s,[1;32],9,true).is_err());assert_eq!(s.outbox.len(),1);snapshot(&s,"test.snap").unwrap();assert_eq!(restore("test.snap").unwrap(),s);let mut bytes=s.encode();bytes[8]^=1;assert!(State::decode(bytes).is_err());println!("snapshot");}""",
    )
    assert "snapshot" in out


def test_eager_boolean_operands_and_empty_fold_are_atomic(tmp_path):
    out = run_rust(
        tmp_path,
        """
module Evaluation;
table T capacity 2 {key id:id64<T>;col v:i64;}
transition eager(k:id64<T>,zero:i64) reads [] writes [T[k].v]
require live:table_exists(T,k);
body {table_set(T,k,v,9);guard(and(false,eq(div(1,zero),0)),ERR_FALSE);commit;}
transition fold(k:id64<T>) reads [] writes [T[k].v]
require live:table_exists(T,k);
body {let %x:i64=fold_bounded<2>(acc:i64=7,i in [3..1]) {yield div(acc,0);};table_set(T,k,v,%x);commit;}
""",
        """fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;assert_eq!(eager(&mut s,2,0),Err(ErrorCode::ArithOverflow));assert_eq!(s.t.col_v[0],0);fold(&mut s,2).unwrap();assert_eq!(s.t.col_v[0],7);println!("evaluation");}""",
    )
    assert "evaluation" in out


def test_rust_projection_exposes_rows_and_parameter_bounds(tmp_path):
    out = run_rust(
        tmp_path,
        """
module View;
table T capacity 2 {key id:id64<T>;col v:i64;}
transition put(k:id64<T>,amount:i64) reads [] writes [T[k].v]
require live:table_exists(T,k);require amountpositive:gt(amount,0);require amountmax:le(amount,100);
body {table_set(T,k,v,amount);commit;}
projection View(k:id64<T>) {allow_if:table_exists(T,k);expose:[T.v[k],T.v];enable_intents:[put when (ge(T[k].v,0))];}
""",
        """fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;s.t.col_v[0]=7;let frame=project_View(&s,2).unwrap();assert!(frame.allowed);assert_eq!(frame.cells.len(),2);assert_eq!(frame.cells[0].value,Some(ProjectionValue::Integer(I256::from_i128(7))));let a=&frame.affordances[0];assert!(a.enabled);let bound=a.bounds.iter().find(|b|b.parameter=="amount").unwrap();assert_eq!(bound.min,Some(ProjectionValue::Integer(I256::from_i128(1))));assert_eq!(bound.max,Some(ProjectionValue::Integer(I256::from_i128(100))));assert!(!project_View(&s,3).unwrap().allowed);println!("projection");}""",
    )
    assert "projection" in out


def test_names_do_not_collide_with_rust_or_case_folding(tmp_path):
    out = run_rust(
        tmp_path,
        """
module Names;
table T capacity 2 {key id:id64<T>;col v:u64;}
table t capacity 2 {key id:id64<t>;col v:u64;}
transition foo_bar(self:u64,vera_self:u64,type:u64) reads [] writes [] body {commit;}
transition fooBar(crate:u64,super:u64) reads [] writes [] body {commit;}
transition snapshot() reads [] writes [] body {commit;}
""",
        """fn main(){let mut s=State::new();foo_bar(&mut s,1,2,3).unwrap();fooBar(&mut s,4,5).unwrap();vera_transition_snapshot(&mut s).unwrap();println!("names");}""",
    )
    assert "names" in out


def test_table_column_pairs_have_distinct_rust_accessors_and_indexes(tmp_path):
    out = run_rust(
        tmp_path,
        """module Names;
table A capacity 2 {key id:id64<A>;col b_c:u64 [unique];col b_d:i64 [tracked_sum];}
table A_b capacity 2 {key id:id64<A_b>;col c:u64 [unique];col d:i64 [tracked_sum];}
transition create() reads [A[*].b_c,A[*].b_d,A_b[*].c,A_b[*].d]
writes [A[*].b_c,A[*].b_d,A_b[*].c,A_b[*].d]
body {
  let %a:id64<A> =table_alloc(A,{b_c:7,b_d:11});
  let %b:id64<A_b> =table_alloc(A_b,{c:9,d:13});
  guard(table_exists_unique(A,b_c,7),ERR_MISSING);
  guard(table_exists_unique(A_b,c,9),ERR_MISSING);
  let %ka:id64<A> =table_lookup(A,b_c,7);
  let %kb:id64<A_b> =table_lookup(A_b,c,9);
  let %va:i64=table_get(A,%ka,b_d);
  let %vb:i64=table_get(A_b,%kb,d);
  table_set(A,%a,b_d,%vb);table_set(A_b,%b,d,%va);commit;
}""",
        """fn main(){let mut s=State::new();create(&mut s).unwrap();
assert_eq!(s.a.col_b_d[0],13);assert_eq!(s.a_b.col_d[0],11);
snapshot(&s,"names.snap").unwrap();assert_eq!(restore("names.snap").unwrap(),s);
println!("distinct");}""",
    )
    assert "distinct" in out


def test_allocation_column_names_cannot_shadow_rust_runtime_variables(tmp_path):
    out = run_rust(
        tmp_path,
        """module Names;
table T capacity 2 {key id:id64<T>;col t:u64;col slot:u64;col gen:u64;}
transition create() reads [] writes [T[*].t,T[*].slot,T[*].gen]
body {let %new:id64<T> =table_alloc(T,{t:7,slot:99,gen:123});commit;}""",
        """fn main(){let mut s=State::new();create(&mut s).unwrap();
assert_eq!(s.t.col_t[0],7);assert_eq!(s.t.col_slot[0],99);
assert_eq!(s.t.col_gen[0],123);assert_eq!(s.t.generation[0],1);
println!("columns");}""",
    )
    assert "columns" in out


def test_capacity_one_handles_and_soa_alignment(tmp_path):
    out = run_rust(
        tmp_path,
        """
module One;
table T capacity 1 {key id:id64<T>;col v:u8;col total:i64 [tracked_sum];}
transition create() reads [] writes [T[*].v,T[*].total]
body {let %k:id64<T> =table_alloc(T,{v:5,total:7});commit;}
""",
        """fn main(){let mut s=State::new();create(&mut s).unwrap();assert!(table_exists_T(&s.t,1));assert!(!table_exists_T(&s.t,0));assert_eq!((&s.t.col_v as *const _ as usize)%64,0);assert_eq!((&s.t.col_total as *const _ as usize)%64,0);assert_eq!((&s.t.agg_sum_total as *const _ as usize)%64,0);assert_eq!(State::decode(s.encode()).unwrap(),s);println!("aligned");}""",
    )
    assert "aligned" in out


def test_aggregate_decimal_scaling_and_scalar_narrowing(tmp_path):
    out = run_rust(
        tmp_path,
        """
module AggregateMath;
table T capacity 2 {key id:id64<T>;col v:dec128 [tracked_sum];}
transition create(value:dec128) reads [] writes [T[*].v]
body {let %k:id64<T> =table_alloc(T,{v:value});commit;}
transition check(scale:dec128) reads [T[*].v] writes []
body {guard(eq(mul(sum(T.v),scale),sum(T.v)),ERR_WRONG);commit;}
transition narrow(k:id64<T>) reads [T[*].v] writes [T[k].v]
require live:table_exists(T,k);
body {let %x:dec128=sum(T.v);table_set(T,k,v,%x);commit;}
""",
        """fn main(){let mut s=State::new();create(&mut s,i128::MAX).unwrap();create(&mut s,i128::MAX).unwrap();check(&mut s,1000000000000000000).unwrap();assert_eq!(narrow(&mut s,2),Err(ErrorCode::ArithOverflow));let a=I256::from_le_bytes([255u8;32]).checked_neg().unwrap();let huge=I256(U256([0,0,0,1u64<<60]));let scaled=decimal_wide_mul(huge,I256::from_i128(1000000000000000000),1000000000000000000).unwrap();assert_eq!(scaled,huge);assert_eq!(a,I256::from_i128(1));println!("aggregate-math");}""",
    )
    assert "aggregate-math" in out


def test_snapshot_detects_valid_domain_bit_corruption(tmp_path):
    out = run_rust(
        tmp_path,
        """
module Integrity;
table T capacity 2 {key id:id64<T>;col v:u64;}
transition put() reads [] writes [] body {commit;}
""",
        """fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;s.t.col_v[0]=7;let mut bytes=s.encode();let at=8+32+8+8;bytes[at]^=1;assert!(State::decode(bytes).is_err());println!("integrity");}""",
    )
    assert "integrity" in out


def test_storage_only_program_and_pure_transition_program_compile(tmp_path):
    out = run_rust(
        tmp_path,
        """
module Storage;
table T capacity 1 {key id:id64<T>;col value:u64;}
""",
        """fn main(){let s=State::new();assert_eq!(State::decode(s.encode()).unwrap(),s);println!("storage");}""",
    )
    assert "storage" in out
    out = run_rust(
        tmp_path,
        """
module Pure;
transition ping(value:u64) reads [] writes [] body {guard(eq(value,7),ERR_BAD);commit;}
""",
        """fn main(){let mut s=State::new();ping(&mut s,7).unwrap();assert!(ping(&mut s,8).is_err());println!("pure");}""",
    )
    assert "pure" in out


def test_allocation_fields_evaluate_in_source_order(tmp_path):
    out = run_rust(
        tmp_path,
        """
module FieldOrder;
table T capacity 2 {key id:id64<T>;col v:i64;}
table R capacity 2 {key id:id64<R>;col first:i64;col second:i64;}
transition build(k:id64<T>) reads [T[k].v] writes [T[k].v,R[*].first,R[*].second]
require live:table_exists(T,k);
body {let %r:id64<R> =table_alloc(R,{second:fold_bounded<1>(acc:i64=0,i in [0..0]) {table_set(T,k,v,7);yield 0;},first:table_get(T,k,v)});commit;}
""",
        """fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;build(&mut s,2).unwrap();assert_eq!(s.r.col_first[0],7);assert_eq!(s.r.col_second[0],0);println!("field-order");}""",
    )
    assert "field-order" in out


def test_read_key_expression_can_mutate_staged_state(tmp_path):
    out = run_rust(
        tmp_path,
        """
module KeyExpression;
table T capacity 2 {key id:id64<T>;col v:i64;}
transition change(k:id64<T>) reads [T[*].v] writes [T[k].v]
require live:table_exists(T,k);
body {let %v:i64=table_get(T,fold_bounded<1>(acc:id64<T> =k,i in [0..0]) {table_set(T,k,v,9);yield acc;},v);guard(eq(%v,9),ERR_VALUE);commit;}
""",
        """fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;change(&mut s,2).unwrap();assert_eq!(s.t.col_v[0],9);println!("key-expression");}""",
    )
    assert "key-expression" in out


def test_old_key_fold_reads_pre_state_in_local_bindings(tmp_path):
    out = run_rust(
        tmp_path,
        """
module OldFold;
table T capacity 4 {key id:id64<T>;col owner:id64<T>;col value:i64;}
transition change(k:id64<T>,to:id64<T>) reads [T[*].owner,T[*].value] writes [T[k].owner,T[k].value]
require live:table_exists(T,k);
body {table_set(T,k,owner,to);let %v:i64=old(T[fold_bounded<1>(acc:id64<T> =k,i in [0..0]) {let %owner:id64<T> =table_get(T,k,owner);guard(true,ERR_OLD_FOLD);yield %owner;}].value);table_set(T,k,value,%v);commit;}
""",
        """fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=7;s.t.generation[0]=1;s.t.generation[1]=1;s.t.generation[2]=1;s.t.col_owner[0]=5;s.t.col_value[1]=20;s.t.col_value[2]=30;change(&mut s,4,6).unwrap();assert_eq!(s.t.col_owner[0],6);assert_eq!(s.t.col_value[0],20);println!("old-context");}""",
    )
    assert "old-context" in out


def test_aggregate_fold_bounds_narrow_at_index_binding(tmp_path):
    out = run_rust(
        tmp_path,
        """
module AggregateBounds;
table T capacity 2 {key id:id64<T>;col value:u8 [tracked_sum];}
transition count() reads [T[*].value] writes []
body {let %x:i64=fold_bounded<2>(acc:i64=0,i in [sum(T.value)..sum(T.value)]) {yield add(acc,1);};guard(eq(%x,1),ERR_COUNT);commit;}
""",
        """fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;s.t.col_value[0]=3;s.t.agg_sum_value=3;count(&mut s).unwrap();s.t.occupied_bitmap[0]=3;s.t.generation[1]=1;s.t.col_value[0]=255;s.t.col_value[1]=1;s.t.agg_sum_value=256;assert_eq!(count(&mut s),Err(ErrorCode::ArithOverflow));println!("index-binding");}""",
    )
    assert "index-binding" in out
