//! Actual native WebGPU dispatch. Software Mesa adapters are sufficient.
use std::{
    future::Future,
    sync::{mpsc, Arc, Mutex, OnceLock},
    task::{Context, Poll, Wake, Waker},
    thread,
    time::Duration,
};
use vera_compiler::{codegen_gpu::generate_wgsl, parse_source, semantic::analyze};

struct ThreadWake(thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => thread::park(),
        }
    }
}
struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    dispatch: Mutex<()>,
}
fn required() -> bool {
    std::env::var("VERA_REQUIRE_GPU_TESTS").is_ok_and(|v| v == "1")
}
fn gpu() -> Option<&'static Gpu> {
    static GPU: OnceLock<Option<Gpu>> = OnceLock::new();
    GPU.get_or_init(|| {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::GL,
            ..Default::default()
        });
        let adapter = match block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            force_fallback_adapter: true,
            ..Default::default()
        })) {
            Ok(adapter) => adapter,
            Err(error) => {
                assert!(!required(), "required WGSL adapter is unavailable: {error}");
                eprintln!("WGSL execution skipped: no GPU/software adapter: {error}");
                return None;
            }
        };
        eprintln!("WGSL conformance adapter: {:?}", adapter.get_info());
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .expect("WebGPU adapter must create a compute device");
        Some(Gpu {
            device,
            queue,
            dispatch: Mutex::new(()),
        })
    })
    .as_ref()
}
fn run(source: &str, probe: &str, entry: &str) -> Option<[u32; 4]> {
    let gpu = gpu()?;
    let _lock = gpu.dispatch.lock().unwrap();
    let program = analyze(parse_source(source).unwrap()).unwrap();
    let code = generate_wgsl(&program, 2).unwrap() + probe;
    let module = gpu
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("native VERA WGSL"),
            source: wgpu::ShaderSource::Wgsl(code.into()),
        });
    let pipeline = gpu
        .device
        .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("VERA conformance"),
            layout: None,
            module: &module,
            entry_point: Some(entry),
            compilation_options: Default::default(),
            cache: None,
        });
    let output = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("VERA counters"),
        size: 16,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("VERA readback"),
        size: 16,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: output.as_entire_binding(),
        }],
    });
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &group, &[]);
        pass.dispatch_workgroups(1, 1, 1);
    }
    encoder.copy_buffer_to_buffer(&output, 0, &readback, 0, 16);
    gpu.queue.submit([encoder.finish()]);
    let (sender, receiver) = mpsc::sync_channel(1);
    readback
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            sender.send(result).unwrap();
        });
    gpu.device.poll(wgpu::PollType::Wait).unwrap();
    receiver
        .recv_timeout(Duration::from_secs(60))
        .expect("GPU readback timed out")
        .expect("GPU readback failed");
    let mapped = readback.slice(..).get_mapped_range();
    let mut values = [0u32; 4];
    for (value, bytes) in values.iter_mut().zip(mapped.chunks_exact(4)) {
        *value = u32::from_le_bytes(bytes.try_into().unwrap());
    }
    drop(mapped);
    readback.unmap();
    Some(values)
}
fn expect(source: &str, probe: &str, entry: &str, expected: [u32; 4]) {
    if let Some(result) = run(source, probe, entry) {
        assert_eq!(result, expected);
    }
}

#[test]
fn wide_unique_allocation_rollback_and_effects_execute() {
    expect(
        r#"table T capacity 2 {key id:id64<T>;col v:dec128 [tracked_sum];col code:bytes32 [unique];}
invariant positive {forall k in T:ge(T[k].v,0);}
effect Created(code:bytes32);
transition create(value:dec128,code:bytes32,accept:bool) reads [] writes [T[*].v,T[*].code]
require positive:ge(value,0);
body {let %k:id64<T>=table_alloc(T,{v:value,code:code});emit Created(code);guard(accept,ERR_REJECT);commit;}
"#,
        r#"
@compute @workgroup_size(1)
fn probe(){var ctx:Ctx;vera_init(&ctx);
var value:V=v_make(42u,0u,0u,16u,0u,0u,0u,0u);
var code:V=v_make(7u,0u,0u,0u,0u,0u,0u,123u);
var rollback:u32=transition_v_create(&ctx,value,code,v_zero());
var status:u32=transition_v_create(&ctx,value,code,v_small(1u));
var duplicate:u32=transition_v_create(&ctx,value,code,v_small(1u));
atomicStore(&output.violations,status+rollback+duplicate);
atomicStore(&output.committed,ctx.s.cols[0].w[3]);
atomicStore(&output.reverted,ctx.s.cols[2].w[7]);
atomicStore(&output.effects,ctx.s.effect_count);}
"#,
        "probe",
        [1, 16, 123, 1],
    );
}

#[test]
fn old_key_folds_and_ensures_use_the_pre_state() {
    expect(
        r#"table T capacity 2 {key id:id64<T>;col owner:id64<T>;col v:i64;col result:i64;}
transition go(k:id64<T>) reads [T[*].owner,T[*].v] writes [T[k].owner,T[k].result]
require live:table_exists(T,k);
ensure snapshot:eq(T[k].result,old(T[table_get(T,k,owner)].v));
body {table_set(T,k,owner,key_of<T>(3));let %v:i64=old(T[fold_bounded<1>(a:id64<T>=k,i in [0..0]) {let %owner:id64<T>=table_get(T,k,owner);guard(true,ERR_REJECT);yield %owner;}].v);table_set(T,k,result,%v);commit;}
"#,
        r#"@compute @workgroup_size(1)
fn probe(){var ctx:Ctx;vera_init(&ctx);ctx.s.occ[0]=1u;ctx.s.gen[0]=1u;ctx.s.cols[0]=v_small(2u);ctx.s.cols[2]=v_small(7u);
var status:u32=transition_v_go(&ctx,v_small(2u));
atomicStore(&output.violations,status);atomicStore(&output.committed,ctx.s.cols[4].w[0]);
atomicStore(&output.reverted,ctx.s.cols[0].w[0]);atomicStore(&output.effects,ctx.trap);}
"#,
        "probe",
        [1, 7, 3, 0],
    );
}

#[test]
fn widened_sums_and_scalar_traps_execute() {
    expect(
        r#"table T capacity 2 {key id:id64<T>;col v:u64 [tracked_sum];}
effect Seen(value:u64);
transition fill() reads [] writes [T[*].v] body {let %k:id64<T>=table_alloc(T,{v:18446744073709551615});commit;}
transition wide() reads [T[*].v] writes [] body {guard(gt(add(sum(T.v),1),sum(T.v)),ERR_BAD);commit;}
transition narrow() reads [T[*].v] writes [] body {emit Seen(sum(T.v));commit;}
"#,
        r#"@compute @workgroup_size(1)
fn probe(){var ctx:Ctx;vera_init(&ctx);var a:u32=transition_v_fill(&ctx);var b:u32=transition_v_fill(&ctx);
var wide:u32=transition_v_wide(&ctx);var narrow:u32=transition_v_narrow(&ctx);
atomicStore(&output.violations,a+b);atomicStore(&output.committed,wide);
atomicStore(&output.reverted,narrow);atomicStore(&output.effects,ctx.s.effect_count);}
"#,
        "probe",
        [2, 1, 3, 0],
    );
}

#[test]
fn exact_512_bit_math_executes_without_native_int64() {
    expect(
        "module Arithmetic;",
        r#"@compute @workgroup_size(1)
fn probe(){
var product:R=v_arith(v_make(0u,0u,0u,0u,0u,0u,256u,0u),v_make(0u,256u,0u,0u,0u,0u,0u,0u),2u,256u,false,v_small(1u));
var quotient:R=v_arith(v_make(0u,0u,0u,0u,0u,0u,0u,65536u),v_make(0u,0u,0u,16u,0u,0u,0u,0u),3u,256u,false,v_small(1u));
var signed_rem:R=v_arith(v_make(4294967289u,4294967295u,4294967295u,4294967295u,4294967295u,4294967295u,4294967295u,4294967295u),v_small(3u),4u,64u,true,v_small(1u));
var inexact:R=v_arith(v_make(2808348672u,232830643u,0u,0u,0u,0u,0u,0u),v_make(4130078720u,698491930u,0u,0u,0u,0u,0u,0u),3u,128u,true,v_make(2808348672u,232830643u,0u,0u,0u,0u,0u,0u));
atomicStore(&output.violations,v_bool(product.ok && product.value.w[7]==65536u).w[0]);
atomicStore(&output.committed,v_bool(quotient.ok && quotient.value.w[4]==4096u).w[0]);
atomicStore(&output.reverted,v_bool(signed_rem.ok && signed_rem.value.w[0]==4294967295u && signed_rem.value.w[7]==4294967295u).w[0]);
atomicStore(&output.effects,v_bool(!inexact.ok).w[0]);}
"#,
        "probe",
        [1, 1, 1, 1],
    );
}

#[test]
fn furnace_counts_invariant_failures_and_unhandled_traps() {
    expect(
        r#"table T capacity 1 {key id:id64<T>;col v:u64;}
invariant zero {forall k in T:and(eq(T[k].v,0),eq(rem(T[k].v,2),0));}
transition bad() reads [] writes [T[*].v] body {let %k:id64<T>=table_alloc(T,{v:1});commit;}
"#,
        "",
        "furnace",
        [32, 32, 0, 0],
    );
    expect(
        "transition bad() reads [] writes [] body {let %x:u64=div(1,0);commit;}",
        "",
        "furnace",
        [32, 0, 32, 0],
    );
}

#[test]
fn bytes32_random_draws_match_the_other_gpu_targets() {
    expect(
        "module Random;",
        r#"@compute @workgroup_size(1)
fn probe(){var ctx:Ctx;vera_init(&ctx);ctx.rng[0]=1u;ctx.rng[1]=2u;ctx.rng[2]=3u;ctx.rng[3]=4u;
var value:V=vera_sample_bytes32(&ctx);
atomicStore(&output.violations,v_bool(v_eq(value,v_make(11520u,0u,5927040u,70819200u,2031721883u,1637235492u,1287239034u,3734860849u))).w[0]);
atomicStore(&output.committed,ctx.rng[0]);atomicStore(&output.reverted,ctx.rng[1]);atomicStore(&output.effects,ctx.rng[3]);}
"#,
        "probe",
        [1, 857776784, 3957087773, 3837013768],
    );
}
