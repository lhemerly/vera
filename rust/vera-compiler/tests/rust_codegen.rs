//! Standalone native Rust lowering: behavior retained from the existing engine contract.
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use vera_compiler::codegen_rust::{generate_rust, RustOptions};
use vera_compiler::parser::parse_source;
use vera_compiler::semantic::analyze;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn run_case(source: &str, main: &str) {
    run_case_with_options(
        source,
        main,
        RustOptions {
            use_rt: false,
            include_main: false,
        },
    );
}

fn run_case_with_options(source: &str, main: &str, options: RustOptions) -> String {
    let folder = Scratch(std::env::temp_dir().join(format!(
        "vera-native-rust-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )));
    fs::create_dir_all(&folder.0).unwrap();
    let program = analyze(parse_source(source).unwrap()).unwrap();
    let code = generate_rust(&program, options).unwrap();
    let path = folder.0.join("case.rs");
    fs::write(&path, format!("{code}\n{main}")).unwrap();
    let binary = folder.0.join("case");
    let mut compiler = Command::new("rustc");
    compiler.args(["--edition", "2021", "-O"]);
    if options.use_rt {
        let dependencies = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let runtime = fs::read_dir(&dependencies)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with("libvera_rt-") && name.ends_with(".rlib")
            })
            .expect("Cargo builds the generated engine's vera-rt dependency");
        compiler
            .arg("--extern")
            .arg(format!("vera_rt={}", runtime.path().display()));
        compiler
            .arg("-L")
            .arg(format!("dependency={}", dependencies.display()));
    }
    let built = compiler.arg(&path).arg("-o").arg(&binary).output().unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let result = Command::new(binary)
        .current_dir(&folder.0)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

#[test]
fn rejected_write_restores_state() {
    run_case(
        r##"
module Atomic;
table T capacity 2 { key id: id64<T>; col v:u64; }
transition change(k:id64<T>, value:u64, accept:bool) reads [] writes [T[k].v]
require live: table_exists(T,k);
body { table_set(T,k,v,value); guard(accept,ERR_REJECT); commit; }
"##,
        r##"fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;s.t.col_v[0]=7;assert!(change(&mut s,2,99,false).is_err());assert_eq!(s.t.col_v[0],7);println!("atomic");}"##,
    );
}

#[test]
fn widened_sums_do_not_wrap() {
    run_case(
        r##"
module Sums;
table T capacity 2 {key id:id64<T>;col v:i64 [tracked_sum];}
transition fill() reads [] writes [T[*].v]
body {let %k:id64<T> =table_alloc(T,{v:9223372036854775807});commit;}
"##,
        r##"fn main(){let mut s=State::new();fill(&mut s).unwrap();fill(&mut s).unwrap();assert_eq!(s.t.agg_sum_v,18446744073709551614i128);println!("wide");}"##,
    );
}

#[test]
fn batch_matches_input_order_and_replay() {
    run_case(
        r##"
module Batch;
table T capacity 2 {key id:id64<T>;col v:u64;}
transition put(k:id64<T>,value:u64) reads [] writes [T[k].v]
require live:table_exists(T,k); body {table_set(T,k,v,value);commit;}
transition copy(src:id64<T>,dst:id64<T>) reads [T[src].v] writes [T[dst].v]
require a:table_exists(T,src);require b:table_exists(T,dst);
body {let %v:u64=table_get(T,src,v);table_set(T,dst,v,%v);commit;}
"##,
        r##"fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=3;s.t.generation[0]=1;s.t.generation[1]=1;let mut replay=s.clone();let intents=vec![Params::Put{k:2,value:1},Params::Copy{src:2,dst:3},Params::Put{k:3,value:2}];assert!(run_batch(&mut s,&intents).iter().all(Result::is_ok));for p in &intents {let (id,args)=p.encode();Params::decode(id,args).unwrap().execute(&mut replay).unwrap();}assert_eq!(s.t.col_v,[1,2]);assert_eq!(s,replay);println!("ordered");}"##,
    );
}

#[test]
fn all_parameter_bytes_survive_codec() {
    run_case(
        r##"
module Codec;
table T capacity 2 {key id:id64<T>;col v:bytes32;}
transition put(value:bytes32,flag:bool,a:u8,b:u16,c:u32,d:dec128) reads [] writes [T[*].v]
body {let %k:id64<T> =table_alloc(T,{v:value});commit;}
"##,
        r##"fn main(){let p=Params::Put{value:[171;32],flag:true,a:255,b:65535,c:4294967295,d:i128::MIN};let (id,args)=p.encode();assert_eq!(Params::decode(id,&args).unwrap(),p);assert!(Params::decode(id,&args[..args.len()-1]).is_err());assert!(Params::decode(u32::MAX,args).is_err());println!("codec");}"##,
    );
}

#[test]
fn effects_and_old_state_are_transactional() {
    run_case(
        r##"
module Effects;
table T capacity 2 {key id:id64<T>;col v:i64 [tracked_sum];}
effect Notice(before:i64,after:i64);
transition change(k:id64<T>,accept:bool) reads [T[k].v] writes [T[k].v]
require live:table_exists(T,k);
body {table_set(T,k,v,9);emit Notice(old(T[k].v),table_get(T,k,v));guard(accept,ERR_REJECT);commit;}
"##,
        r##"fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;s.t.col_v[0]=7;s.t.agg_sum_v=7;assert!(change(&mut s,2,false).is_err());assert!(s.outbox.is_empty());change(&mut s,2,true).unwrap();assert_eq!(s.outbox,vec![Effect::Notice{before:7,after:9}]);println!("effects");}"##,
    );
}

#[test]
fn checked_arithmetic_and_lazy_select() {
    run_case(
        r##"
module Arithmetic;
table T capacity 2 {key id:id64<T>;col v:u8;}
transition put(k:id64<T>,a:u8,b:u8) reads [] writes [T[k].v]
require live:table_exists(T,k);
body {let %x:u8=select(eq(b,0),min(a,9),max(add(a,b),7));table_set(T,k,v,%x);commit;}
"##,
        r##"fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;put(&mut s,2,255,0).unwrap();assert_eq!(s.t.col_v[0],9);assert!(put(&mut s,2,255,1).is_err());assert_eq!(s.t.col_v[0],9);println!("checked");}"##,
    );
}

#[test]
fn fold_index_does_not_wrap_at_type_max() {
    run_case(
        r##"
module Fold;
table T capacity 2 {key id:id64<T>;col v:i64;}
transition put(k:id64<T>,lo:u8,hi:u8) reads [] writes [T[k].v]
require live:table_exists(T,k);
body {let %x:i64=fold_bounded<4>(acc:i64=0,i in [lo..hi]) {yield add(acc,1);};table_set(T,k,v,%x);commit;}
"##,
        r##"fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;put(&mut s,2,254,255).unwrap();assert_eq!(s.t.col_v[0],2);println!("fold");}"##,
    );
}

#[test]
fn dec128_full_width_arithmetic_and_aggregate() {
    run_case(
        r##"
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
"##,
        r##"fn main(){let mut s=State::new();create(&mut s,i128::MAX).unwrap();create(&mut s,i128::MAX).unwrap();assert_eq!(format!("{}",s.t.agg_sum_v),"340282366920938463463374607431768211454");product(&mut s,2,i128::MAX,1000000000000000000).unwrap();assert_eq!(s.t.col_v[0],i128::MAX);quotient(&mut s,2,i128::MIN,1000000000000000000).unwrap();assert_eq!(s.t.col_v[0],i128::MIN);assert!(quotient(&mut s,2,i128::MIN,-1000000000000000000).is_err());assert_eq!(s.t.col_v[0],i128::MIN);assert_eq!(State::decode(s.encode()).unwrap(),s);println!("decimal128");}"##,
    );
}

#[test]
fn unique_index_and_snapshot_restore() {
    run_case(
        r##"
module UniqueSnapshot;
table T capacity 4 {key id:id64<T>;col code:bytes32 [unique];col v:i64 [tracked_sum];}
effect Created(code:bytes32);
transition create(code:bytes32,value:i64,accept:bool) reads [] writes [T[*].code,T[*].v]
body {let %k:id64<T> =table_alloc(T,{code:code,v:value});emit Created(code);guard(accept,ERR_REJECT);commit;}
transition find(code:bytes32) reads [T[*].code,T[*].v] writes []
body {let %k:id64<T> =table_lookup(T,code,code);let %v:i64=table_get(T,%k,v);guard(eq(%v,7),ERR_VALUE);commit;}
"##,
        r##"fn main(){let mut s=State::new();assert!(create(&mut s,[1;32],7,false).is_err());assert_eq!(s,State::new());create(&mut s,[1;32],7,true).unwrap();find(&mut s,[1;32]).unwrap();assert!(create(&mut s,[1;32],9,true).is_err());assert_eq!(s.outbox.len(),1);snapshot(&s,"test.snap").unwrap();assert_eq!(restore("test.snap").unwrap(),s);let mut bytes=s.encode();bytes[8]^=1;assert!(State::decode(bytes).is_err());println!("snapshot");}"##,
    );
}

#[test]
fn eager_boolean_operands_and_empty_fold_are_atomic() {
    run_case(
        r##"
module Evaluation;
table T capacity 2 {key id:id64<T>;col v:i64;}
transition eager(k:id64<T>,zero:i64) reads [] writes [T[k].v]
require live:table_exists(T,k);
body {table_set(T,k,v,9);guard(and(false,eq(div(1,zero),0)),ERR_FALSE);commit;}
transition fold(k:id64<T>) reads [] writes [T[k].v]
require live:table_exists(T,k);
body {let %x:i64=fold_bounded<2>(acc:i64=7,i in [3..1]) {yield div(acc,0);};table_set(T,k,v,%x);commit;}
"##,
        r##"fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;assert_eq!(eager(&mut s,2,0),Err(ErrorCode::ArithOverflow));assert_eq!(s.t.col_v[0],0);fold(&mut s,2).unwrap();assert_eq!(s.t.col_v[0],7);println!("evaluation");}"##,
    );
}

#[test]
fn rust_projection_exposes_rows_and_parameter_bounds() {
    run_case(
        r##"
module View;
table T capacity 2 {key id:id64<T>;col v:i64;}
transition put(k:id64<T>,amount:i64) reads [] writes [T[k].v]
require live:table_exists(T,k);require amountpositive:gt(amount,0);require amountmax:le(amount,100);
body {table_set(T,k,v,amount);commit;}
projection View(k:id64<T>) {allow_if:table_exists(T,k);expose:[T.v[k],T.v];enable_intents:[put when (ge(T[k].v,0))];}
"##,
        r##"fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;s.t.col_v[0]=7;let frame=project_View(&s,2).unwrap();assert!(frame.allowed);assert_eq!(frame.cells.len(),2);assert_eq!(frame.cells[0].value,Some(ProjectionValue::Integer(I256::from_i128(7))));let a=&frame.affordances[0];assert!(a.enabled);let bound=a.bounds.iter().find(|b|b.parameter=="amount").unwrap();assert_eq!(bound.min,Some(ProjectionValue::Integer(I256::from_i128(1))));assert_eq!(bound.max,Some(ProjectionValue::Integer(I256::from_i128(100))));assert!(!project_View(&s,3).unwrap().allowed);println!("projection");}"##,
    );
}

#[test]
fn names_do_not_collide_with_rust_or_case_folding() {
    run_case(
        r##"
module Names;
table T capacity 2 {key id:id64<T>;col v:u64;}
table t capacity 2 {key id:id64<t>;col v:u64;}
transition foo_bar(self:u64,vera_self:u64,type:u64) reads [] writes [] body {commit;}
transition fooBar(crate:u64,super:u64) reads [] writes [] body {commit;}
transition snapshot() reads [] writes [] body {commit;}
"##,
        r##"fn main(){let mut s=State::new();foo_bar(&mut s,1,2,3).unwrap();fooBar(&mut s,4,5).unwrap();vera_transition_snapshot(&mut s).unwrap();println!("names");}"##,
    );
}

#[test]
fn table_column_pairs_have_distinct_rust_accessors_and_indexes() {
    run_case(
        r##"module Names;
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
}"##,
        r##"fn main(){let mut s=State::new();create(&mut s).unwrap();
assert_eq!(s.a.col_b_d[0],13);assert_eq!(s.a_b.col_d[0],11);
snapshot(&s,"names.snap").unwrap();assert_eq!(restore("names.snap").unwrap(),s);
println!("distinct");}"##,
    );
}

#[test]
fn allocation_column_names_cannot_shadow_rust_runtime_variables() {
    run_case(
        r##"module Names;
table T capacity 2 {key id:id64<T>;col t:u64;col slot:u64;col gen:u64;}
transition create() reads [] writes [T[*].t,T[*].slot,T[*].gen]
body {let %new:id64<T> =table_alloc(T,{t:7,slot:99,gen:123});commit;}"##,
        r##"fn main(){let mut s=State::new();create(&mut s).unwrap();
assert_eq!(s.t.col_t[0],7);assert_eq!(s.t.col_slot[0],99);
assert_eq!(s.t.col_gen[0],123);assert_eq!(s.t.generation[0],1);
println!("columns");}"##,
    );
}

#[test]
fn capacity_one_handles_and_soa_alignment() {
    run_case(
        r##"
module One;
table T capacity 1 {key id:id64<T>;col v:u8;col total:i64 [tracked_sum];}
transition create() reads [] writes [T[*].v,T[*].total]
body {let %k:id64<T> =table_alloc(T,{v:5,total:7});commit;}
"##,
        r##"fn main(){let mut s=State::new();create(&mut s).unwrap();assert!(table_exists_T(&s.t,1));assert!(!table_exists_T(&s.t,0));assert_eq!((&s.t.col_v as *const _ as usize)%64,0);assert_eq!((&s.t.col_total as *const _ as usize)%64,0);assert_eq!((&s.t.agg_sum_total as *const _ as usize)%64,0);assert_eq!(State::decode(s.encode()).unwrap(),s);println!("aligned");}"##,
    );
}

#[test]
fn aggregate_decimal_scaling_and_scalar_narrowing() {
    run_case(
        r##"
module AggregateMath;
table T capacity 2 {key id:id64<T>;col v:dec128 [tracked_sum];}
transition create(value:dec128) reads [] writes [T[*].v]
body {let %k:id64<T> =table_alloc(T,{v:value});commit;}
transition check(scale:dec128) reads [T[*].v] writes []
body {guard(eq(mul(sum(T.v),scale),sum(T.v)),ERR_WRONG);commit;}
transition narrow(k:id64<T>) reads [T[*].v] writes [T[k].v]
require live:table_exists(T,k);
body {let %x:dec128=sum(T.v);table_set(T,k,v,%x);commit;}
"##,
        r##"fn main(){let mut s=State::new();create(&mut s,i128::MAX).unwrap();create(&mut s,i128::MAX).unwrap();check(&mut s,1000000000000000000).unwrap();assert_eq!(narrow(&mut s,2),Err(ErrorCode::ArithOverflow));let a=I256::from_le_bytes([255u8;32]).checked_neg().unwrap();let huge=I256(U256([0,0,0,1u64<<60]));let scaled=decimal_wide_mul(huge,I256::from_i128(1000000000000000000),1000000000000000000).unwrap();assert_eq!(scaled,huge);assert_eq!(a,I256::from_i128(1));println!("aggregate-math");}"##,
    );
}

#[test]
fn snapshot_detects_valid_domain_bit_corruption() {
    run_case(
        r##"
module Integrity;
table T capacity 2 {key id:id64<T>;col v:u64;}
transition put() reads [] writes [] body {commit;}
"##,
        r##"fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;s.t.col_v[0]=7;let mut bytes=s.encode();let at=8+32+8+8;bytes[at]^=1;assert!(State::decode(bytes).is_err());println!("integrity");}"##,
    );
}

#[test]
fn storage_only_program_and_pure_transition_program_compile() {
    run_case(
        r##"
module Storage;
table T capacity 1 {key id:id64<T>;col value:u64;}
"##,
        r##"fn main(){let s=State::new();assert_eq!(State::decode(s.encode()).unwrap(),s);println!("storage");}"##,
    );
}

#[test]
fn storage_only_program_and_pure_transition_program_compile_1() {
    run_case(
        r##"
module Pure;
transition ping(value:u64) reads [] writes [] body {guard(eq(value,7),ERR_BAD);commit;}
"##,
        r##"fn main(){let mut s=State::new();ping(&mut s,7).unwrap();assert!(ping(&mut s,8).is_err());println!("pure");}"##,
    );
}

#[test]
fn allocation_fields_evaluate_in_source_order() {
    run_case(
        r##"
module FieldOrder;
table T capacity 2 {key id:id64<T>;col v:i64;}
table R capacity 2 {key id:id64<R>;col first:i64;col second:i64;}
transition build(k:id64<T>) reads [T[k].v] writes [T[k].v,R[*].first,R[*].second]
require live:table_exists(T,k);
body {let %r:id64<R> =table_alloc(R,{second:fold_bounded<1>(acc:i64=0,i in [0..0]) {table_set(T,k,v,7);yield 0;},first:table_get(T,k,v)});commit;}
"##,
        r##"fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;build(&mut s,2).unwrap();assert_eq!(s.r.col_first[0],7);assert_eq!(s.r.col_second[0],0);println!("field-order");}"##,
    );
}

#[test]
fn read_key_expression_can_mutate_staged_state() {
    run_case(
        r##"
module KeyExpression;
table T capacity 2 {key id:id64<T>;col v:i64;}
transition change(k:id64<T>) reads [T[*].v] writes [T[k].v]
require live:table_exists(T,k);
body {let %v:i64=table_get(T,fold_bounded<1>(acc:id64<T> =k,i in [0..0]) {table_set(T,k,v,9);yield acc;},v);guard(eq(%v,9),ERR_VALUE);commit;}
"##,
        r##"fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;change(&mut s,2).unwrap();assert_eq!(s.t.col_v[0],9);println!("key-expression");}"##,
    );
}

#[test]
fn old_key_fold_reads_pre_state_in_local_bindings() {
    run_case(
        r##"
module OldFold;
table T capacity 4 {key id:id64<T>;col owner:id64<T>;col value:i64;}
transition change(k:id64<T>,to:id64<T>) reads [T[*].owner,T[*].value] writes [T[k].owner,T[k].value]
require live:table_exists(T,k);
body {table_set(T,k,owner,to);let %v:i64=old(T[fold_bounded<1>(acc:id64<T> =k,i in [0..0]) {let %owner:id64<T> =table_get(T,k,owner);guard(true,ERR_OLD_FOLD);yield %owner;}].value);table_set(T,k,value,%v);commit;}
"##,
        r##"fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=7;s.t.generation[0]=1;s.t.generation[1]=1;s.t.generation[2]=1;s.t.col_owner[0]=5;s.t.col_value[1]=20;s.t.col_value[2]=30;change(&mut s,4,6).unwrap();assert_eq!(s.t.col_owner[0],6);assert_eq!(s.t.col_value[0],20);println!("old-context");}"##,
    );
}

#[test]
fn aggregate_fold_bounds_narrow_at_index_binding() {
    run_case(
        r##"
module AggregateBounds;
table T capacity 2 {key id:id64<T>;col value:u8 [tracked_sum];}
transition count() reads [T[*].value] writes []
body {let %x:i64=fold_bounded<2>(acc:i64=0,i in [sum(T.value)..sum(T.value)]) {yield add(acc,1);};guard(eq(%x,1),ERR_COUNT);commit;}
"##,
        r##"fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;s.t.col_value[0]=3;s.t.agg_sum_value=3;count(&mut s).unwrap();s.t.occupied_bitmap[0]=3;s.t.generation[1]=1;s.t.col_value[0]=255;s.t.col_value[1]=1;s.t.agg_sum_value=256;assert_eq!(count(&mut s),Err(ErrorCode::ArithOverflow));println!("index-binding");}"##,
    );
}

#[test]
fn signed_aggregate_remainder_preserves_minimum_trap() {
    run_case(
        r##"
module AggregateMinimum;
table T capacity 2 {key id:id64<T>;col v:i64 [tracked_sum];}
transition remainder() reads [T[*].v] writes [] body {
 let %value:i64=rem(add(sum(T.v),-170141183460469231731687303715884105728),-1);
 commit;
}
"##,
        r##"fn main(){let mut s=State::new();assert_eq!(remainder(&mut s),Err(ErrorCode::ArithOverflow));assert_eq!(s,State::new());}"##,
    );
}

#[test]
fn contextual_aggregate_promotions_cover_reads_bindings_and_folds() {
    run_case(
        r##"
module Promotion;
table T capacity 2 {key id:id64<T>;col v:i64 [tracked_sum];}
transition verify(k:id64<T>,value:i64) reads [T[k].v,T[*].v] writes []
 require live:table_exists(T,k);
 body {
 guard(eq(add(sum(T.v),value),14),ERR_BINDING);
 guard(eq(add(sum(T.v),table_get(T,k,v)),14),ERR_GET);
 guard(eq(add(sum(T.v),old(T[k].v)),14),ERR_OLD);
 guard(eq(add(sum(T.v),fold_bounded<1>(acc:i64=7,i in [0..0]) {yield acc;}),14),ERR_FOLD);
 commit;
 }
"##,
        r##"fn main(){let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;s.t.col_v[0]=7;s.t.agg_sum_v=7;verify(&mut s,2,7).unwrap();}"##,
    );
}

#[test]
fn guard_codes_and_underscore_identifiers_have_distinct_rust_names() {
    run_case(
        r##"
module Names;
table _ capacity 1 {key id:id64<_>;col v:u8;}
transition first() reads [] writes [] body {guard(false,self);commit;}
transition second() reads [] writes [] body {guard(false,vera_self);commit;}
transition _() reads [] writes [] body {commit;}
"##,
        r##"fn main(){let mut s=State::new();assert!(first(&mut s).is_err());assert!(second(&mut s).is_err());vera__(&mut s).unwrap();}"##,
    );
}

#[test]
fn reachable_unique_index_orders_are_portable_and_invalid_slots_are_rejected() {
    run_case(
        r##"
module IndexOrder;
table T capacity 4 {key id:id64<T>;col v:u64 [unique];}
transition create(value:u64) reads [] writes [T[*].v]
 body {let %key:id64<T> =table_alloc(T,{v:value});commit;}
"##,
        r##"fn main(){let mut s=State::new();let values=(0u64..).filter(|n|mix64(*n)&15==0).take(3).collect::<Vec<_>>();for value in values{create(&mut s,value).unwrap();}s.t.hash_v_key.swap(0,1);s.validate().unwrap();assert_eq!(State::decode(s.encode()).unwrap(),s);let mut bad=s.clone();bad.t.hash_v_key[0]=4;assert_eq!(bad.validate(),Err(ErrorCode::InvalidEncoding));let mut bad=s.clone();bad.t.hash_v_key[0]=bad.t.hash_v_key[1];assert_eq!(bad.validate(),Err(ErrorCode::InvalidEncoding));}"##,
    );
}

#[test]
fn generated_intents_snapshots_and_wal_match_historical_portable_bytes() {
    let constants = format!(
        "const EXPECT_INTENT:&str={:?};const EXPECT_SNAPSHOT:&str={:?};const EXPECT_WAL:&str={:?};",
        include_str!("fixtures/codec-intent.hex").trim(),
        include_str!("fixtures/codec-snapshot.hex").trim(),
        include_str!("fixtures/codec-wal.hex").trim(),
    );
    let main = r##"
fn hex(bytes:&[u8])->String{bytes.iter().map(|byte|format!("{byte:02x}")).collect()}
fn main(){
 let p=Params::Create{flag:true,tiny:255,small:0x1234,word:0x89abcdef,big:0xfedcba9876543210,signed:i64::MIN,decimal:-123456789,wide:-((1i128<<120)+123),symbol:0xaabbccdd,blob:core::array::from_fn(|i|i as u8),tag:1,peer:u64::MAX};
 let(tag,args)=p.encode();assert_eq!(tag,0);assert_eq!(hex(&args),EXPECT_INTENT);
 let frame=IntentRecord{intent_id:1,tx_seq:1,transition:tag,args:args.clone()}.encode().unwrap();assert_eq!(hex(&frame),EXPECT_WAL);
 let mut state=State::new();p.execute(&mut state).unwrap();assert_eq!(hex(&state.encode()),EXPECT_SNAPSHOT);assert_eq!(State::decode(state.encode()).unwrap(),state);
}
"##;
    run_case(include_str!("fixtures/codec.vera"), &(constants + main));
}

#[test]
fn demo_engine_executes_and_replays_bank_intents() {
    let output = run_case_with_options(
        include_str!("../../../examples/bank.vera"),
        "",
        RustOptions {
            use_rt: false,
            include_main: true,
        },
    );
    assert!(output.contains("wal tx_seq = 3"), "{output}");
    assert!(
        output.contains("replay matches live state: deterministic"),
        "{output}"
    );
}

#[test]
fn shared_runtime_backend_writes_and_replays_typed_intents() {
    run_case_with_options(
        "module NativeRuntime;transition accept(value:u64) reads [] writes [] body {guard(gt(value,0),ERR_VALUE);commit;}",
        r##"fn main(){let p=Params::Accept{value:7};let(tag,args)=p.encode();let mut wal=WAL::open("test.wal").unwrap();wal.append(tag,args).unwrap();wal.sync().unwrap();let records=replay_file("test.wal").unwrap();assert_eq!(records.len(),1);let decoded=Params::decode(records[0].transition,&records[0].args).unwrap();assert_eq!(decoded,p);decoded.execute(&mut State::new()).unwrap();}"##,
        RustOptions { use_rt: true, include_main: false },
    );
}

#[test]
fn transitions_do_not_shadow_generated_wide_constructors_or_projection_helpers() {
    run_case(
        "module Reserved;transition I256() reads [] writes [] body {commit;}transition U256() reads [] writes [] body {commit;}transition projection_bound() reads [] writes [] body {commit;}",
        "fn main(){let mut s=State::new();vera_transition_I256(&mut s).unwrap();vera_transition_U256(&mut s).unwrap();vera_transition_projection_bound(&mut s).unwrap();}",
    );
}

#[test]
fn scalar_arithmetic_matches_exact_integer_semantics_at_domain_edges() {
    use num_bigint::BigInt;
    use vera_compiler::ir::BinaryOp;
    use vera_compiler::types::{Domain, ScalarType};
    use vera_compiler::value::{checked_binary, Value};

    for ty in [
        ScalarType::U8,
        ScalarType::U16,
        ScalarType::U32,
        ScalarType::U64,
        ScalarType::I64,
        ScalarType::Dec64,
        ScalarType::Dec128,
    ] {
        let minimum = ty.min_value();
        let maximum = ty.max_value();
        let scale = BigInt::from(10u8).pow(ty.scale());
        let candidates = vec![
            (0.into(), 0.into()),
            (1.into(), 0.into()),
            (maximum.clone(), 1.into()),
            (maximum.clone(), 2.into()),
            (maximum.clone(), maximum.clone()),
            (minimum.clone(), (-1).into()),
            ((-1).into(), minimum.clone()),
            (minimum.clone(), minimum.clone()),
            (minimum.clone(), 1.into()),
            (maximum.clone(), (-1).into()),
            (1.into(), 2.into()),
            ((-1).into(), 2.into()),
            (2.into(), (-1).into()),
            (scale.clone(), scale.clone()),
            (scale.clone(), 2.into()),
            (1.into(), scale.clone()),
            (scale.clone(), 1.into()),
            (&scale + 1u8, &scale + 1u8),
            (maximum.clone(), scale.clone()),
            (minimum.clone(), scale),
        ];
        let cases = candidates
            .into_iter()
            .filter(|(a, b)| a >= &minimum && a <= &maximum && b >= &minimum && b <= &maximum)
            .collect::<Vec<_>>();
        let operators = [
            BinaryOp::Add,
            BinaryOp::Sub,
            BinaryOp::Mul,
            BinaryOp::Div,
            BinaryOp::Rem,
            BinaryOp::Min,
            BinaryOp::Max,
        ];
        let mut source = format!(
            "module ScalarMatrix;table T capacity 1 {{key id:id64<T>;col value:{};}}",
            ty.name()
        );
        let mut main = String::from("fn main(){");
        for op in operators {
            source.push_str(&format!("transition calc_{}(a:{},b:{}) reads [] writes [T[id64<T>(1)].value] body {{table_set(T,id64<T>(1),value,{}(a,b));commit;}}",op.name(),ty.name(),ty.name(),op.name()));
            let mut inputs = Vec::new();
            let mut expected = Vec::new();
            for (a, b) in &cases {
                inputs.push(format!("({a}{},{b}{})", ty.rust_type(), ty.rust_type()));
                let result = checked_binary(
                    op,
                    &Value::Int(a.clone()),
                    &Value::Int(b.clone()),
                    &Domain::scalar(ty.clone()),
                );
                expected.push(match result {
                    Ok(Value::Int(value)) => format!("Some({value}{})", ty.rust_type()),
                    Err(_) => "None".into(),
                    _ => unreachable!(),
                });
            }
            main.push_str(&format!("for ((a,b),expected) in [{}].into_iter().zip([{}]){{let mut s=State::new();s.t.occupied_bitmap[0]=1;s.t.generation[0]=1;let before=s.clone();let result=calc_{}(&mut s,a,b);match expected{{Some(value)=>{{assert_eq!(result,Ok(()),\"{} {} {{a:?}} {{b:?}}\");assert_eq!(s.t.col_value[0],value);}},None=>{{assert_eq!(result,Err(ErrorCode::ArithOverflow),\"{} {} {{a:?}} {{b:?}}\");assert_eq!(s,before);}}}}}}",inputs.join(","),expected.join(","),op.name(),ty.name(),op.name(),ty.name(),op.name()));
        }
        main.push('}');
        run_case(&source, &main);
    }
}

#[test]
fn aggregate_arithmetic_and_narrowing_match_exact_integer_semantics() {
    use num_bigint::BigInt;
    use vera_compiler::ir::BinaryOp;
    use vera_compiler::types::{Domain, ScalarType};
    use vera_compiler::value::{checked_binary, coerce, Value};

    for ty in [
        ScalarType::U8,
        ScalarType::U16,
        ScalarType::U32,
        ScalarType::U64,
        ScalarType::I64,
        ScalarType::Dec64,
        ScalarType::Dec128,
    ] {
        let minimum = ty.min_value();
        let maximum = ty.max_value();
        let scale = BigInt::from(10u8).pow(ty.scale());
        let candidates = vec![
            (0.into(), 0.into(), 0.into()),
            (0.into(), 0.into(), 1.into()),
            (maximum.clone(), maximum.clone(), maximum.clone()),
            (maximum.clone(), maximum.clone(), minimum.clone()),
            (maximum.clone(), maximum.clone(), 1.into()),
            (maximum.clone(), maximum.clone(), 2.into()),
            (maximum.clone(), maximum.clone(), scale.clone()),
            (minimum.clone(), minimum.clone(), minimum.clone()),
            (minimum.clone(), minimum.clone(), (-1).into()),
            (minimum.clone(), minimum.clone(), 1.into()),
            (minimum.clone(), minimum.clone(), 2.into()),
            (minimum.clone(), minimum.clone(), scale.clone()),
            (maximum.clone(), minimum.clone(), maximum.clone()),
            (maximum.clone(), minimum.clone(), minimum.clone()),
            (scale.clone(), scale.clone(), scale.clone()),
            (scale.clone(), scale.clone(), &scale + 1u8),
            (maximum.clone(), 1.into(), 0.into()),
            (maximum.clone(), 1.into(), 1.into()),
            (0.into(), 1.into(), 2.into()),
        ];
        let cases = candidates
            .into_iter()
            .filter(|(a, b, c)| {
                [a, b, c]
                    .into_iter()
                    .all(|n| n >= &minimum && n <= &maximum)
            })
            .collect::<Vec<_>>();
        let operators = [
            BinaryOp::Add,
            BinaryOp::Sub,
            BinaryOp::Mul,
            BinaryOp::Div,
            BinaryOp::Rem,
            BinaryOp::Min,
            BinaryOp::Max,
        ];
        let mut source=format!("module AggregateMatrix;table T capacity 2 {{key id:id64<T>;col value:{} [tracked_sum];}}",ty.name());
        let mut main = String::from("fn main(){");
        for op in operators {
            source.push_str(&format!("transition calc_{}(rhs:{}) reads [T[*].value] writes [T[id64<T>(2)].value] body {{table_set(T,id64<T>(2),value,{}(sum(T.value),rhs));commit;}}",op.name(),ty.name(),op.name()));
            let mut inputs = Vec::new();
            let mut expected = Vec::new();
            for (a, b, c) in &cases {
                inputs.push(format!(
                    "({a}{},{b}{},{c}{})",
                    ty.rust_type(),
                    ty.rust_type(),
                    ty.rust_type()
                ));
                let result = checked_binary(
                    op,
                    &Value::Int(a + b),
                    &Value::Int(c.clone()),
                    &Domain::aggregate(ty.clone()),
                )
                .and_then(|value| coerce(&value, &ty));
                expected.push(match result {
                    Ok(Value::Int(value)) => format!("Some({value}{})", ty.rust_type()),
                    Err(_) => "None".into(),
                    _ => unreachable!(),
                });
            }
            let aggregate = if ty.bits() > 64 {
                "I256::from_i128(a).checked_add(I256::from_i128(b)).unwrap()"
            } else if ty.signed() {
                "(a as i128)+(b as i128)"
            } else {
                "(a as u128)+(b as u128)"
            };
            main.push_str(&format!("for ((a,b,rhs),expected) in [{}].into_iter().zip([{}]){{let mut s=State::new();s.t.occupied_bitmap[0]=3;s.t.generation=[1,1];s.t.col_value=[a,b];s.t.agg_sum_value={aggregate};let before=s.clone();let result=calc_{}(&mut s,rhs);match expected{{Some(value)=>{{assert_eq!(result,Ok(()),\"{} {} {{a:?}} {{b:?}} {{rhs:?}}\");assert_eq!(s.t.col_value[0],value);s.validate().unwrap();}},None=>{{assert_eq!(result,Err(ErrorCode::ArithOverflow),\"{} {} {{a:?}} {{b:?}} {{rhs:?}}\");assert_eq!(s,before);}}}}}}",inputs.join(","),expected.join(","),op.name(),ty.name(),op.name(),ty.name(),op.name()));
        }
        main.push('}');
        run_case(&source, &main);
    }
}
