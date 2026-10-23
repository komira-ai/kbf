//! The GPU probe a pool user runs: this test binary, started with [`ARG`] (the hook in
//! `tests/slot_gpu.rs` runs it before the test harness). It finds the Metal device,
//! compiles a kernel from source at run time (which needs the system's Metal compiler
//! service), makes a pipeline, runs the kernel over 4096 floats on the GPU and compares
//! every result. One `STAGE <name> ok|FAIL <detail>` line per step, and the exit code
//! says which step failed ([`EXIT`]), so a failure names the step.

use std::io::Write as _;

use objc2_foundation::NSString;
use objc2_metal::{
    MTLBuffer as _, MTLCommandBuffer as _, MTLCommandEncoder as _, MTLCommandQueue as _,
    MTLComputeCommandEncoder as _, MTLComputePipelineState as _, MTLCreateSystemDefaultDevice,
    MTLDevice as _, MTLLibrary as _, MTLResourceOptions, MTLSize,
};

/// The argument after argv0 that selects the probe.
pub const ARG: &str = "__gpu-probe";

/// The steps, in order, with the exit code each one's failure gives.
pub const EXIT: [(&str, i32); 6] = [
    ("device", 3),
    ("queue", 4),
    ("library", 5),
    ("pipeline", 7),
    ("dispatch", 9),
    ("result", 10),
];

/// The kernel: `out[i] = in[i] * in[i] + 1`.
const SOURCE: &str = "#include <metal_stdlib>\n\
using namespace metal;\n\
kernel void square_plus_one(device const float* in [[buffer(0)]],\n\
                            device float* out [[buffer(1)]],\n\
                            uint i [[thread_position_in_grid]]) {\n\
    out[i] = in[i] * in[i] + 1.0f;\n\
}\n";

/// How many floats the kernel runs over.
const COUNT: usize = 4096;

fn stage(name: &str, ok: bool, detail: &str) -> bool {
    println!("STAGE {name} {} {detail}", if ok { "ok" } else { "FAIL" });
    let _ = std::io::stdout().flush();
    ok
}

/// The exit code of a failure at `name`.
fn fail(name: &str) -> i32 {
    EXIT.iter()
        .find(|(step, _)| *step == name)
        .map_or(1, |(_, code)| *code)
}

/// Runs the probe: 0 when every step passed, else the failed step's code.
pub fn run() -> i32 {
    let Some(device) = MTLCreateSystemDefaultDevice() else {
        stage("device", false, "MTLCreateSystemDefaultDevice returned nil");
        return fail("device");
    };
    stage("device", true, &format!("name={}", device.name()));
    let Some(queue) = device.newCommandQueue() else {
        stage("queue", false, "no command queue");
        return fail("queue");
    };
    stage("queue", true, "");
    let library = match device.newLibraryWithSource_options_error(&NSString::from_str(SOURCE), None)
    {
        Ok(library) => library,
        Err(e) => {
            stage("library", false, &e.localizedDescription().to_string());
            return fail("library");
        }
    };
    let Some(function) = library.newFunctionWithName(&NSString::from_str("square_plus_one")) else {
        stage("library", false, "the kernel is not in the library");
        return fail("library");
    };
    stage("library", true, "compiled from source");
    let pipeline = match device.newComputePipelineStateWithFunction_error(&function) {
        Ok(pipeline) => pipeline,
        Err(e) => {
            stage("pipeline", false, &e.localizedDescription().to_string());
            return fail("pipeline");
        }
    };
    stage(
        "pipeline",
        true,
        &format!("maxThreads={}", pipeline.maxTotalThreadsPerThreadgroup()),
    );

    let bytes = COUNT * std::mem::size_of::<f32>();
    let shared = MTLResourceOptions::StorageModeShared;
    let (Some(input), Some(output)) = (
        device.newBufferWithLength_options(bytes, shared),
        device.newBufferWithLength_options(bytes, shared),
    ) else {
        stage("dispatch", false, "no buffers");
        return fail("dispatch");
    };
    // SAFETY: `input` is a shared buffer of `COUNT` floats that no command is using yet.
    let values =
        unsafe { std::slice::from_raw_parts_mut(input.contents().as_ptr().cast::<f32>(), COUNT) };
    for (i, value) in values.iter_mut().enumerate() {
        *value = (i % 17) as f32;
    }
    let Some(commands) = queue.commandBuffer() else {
        stage("dispatch", false, "no command buffer");
        return fail("dispatch");
    };
    let Some(encoder) = commands.computeCommandEncoder() else {
        stage("dispatch", false, "no encoder");
        return fail("dispatch");
    };
    encoder.setComputePipelineState(&pipeline);
    // SAFETY: both buffers hold `COUNT` floats, as the kernel reads and writes.
    unsafe {
        encoder.setBuffer_offset_atIndex(Some(&input), 0, 0);
        encoder.setBuffer_offset_atIndex(Some(&output), 0, 1);
    }
    let grid = MTLSize {
        width: COUNT,
        height: 1,
        depth: 1,
    };
    let group = MTLSize {
        width: 64,
        height: 1,
        depth: 1,
    };
    encoder.dispatchThreads_threadsPerThreadgroup(grid, group);
    encoder.endEncoding();
    commands.commit();
    commands.waitUntilCompleted();
    let completed = commands.error().is_none();
    if !stage(
        "dispatch",
        completed,
        &format!("status={:?}", commands.status()),
    ) {
        return fail("dispatch");
    }
    // SAFETY: the command buffer has completed, so the GPU is not writing `output`.
    let results =
        unsafe { std::slice::from_raw_parts(output.contents().as_ptr().cast::<f32>(), COUNT) };
    let wrong = results
        .iter()
        .enumerate()
        .filter(|(i, got)| **got != ((i % 17) * (i % 17) + 1) as f32)
        .count();
    if !stage("result", wrong == 0, &format!("mismatches={wrong}/{COUNT}")) {
        return fail("result");
    }
    // What the GPU run left in the user's per-user folders (Metal keeps its shader cache
    // under the cache folder): the clean of the slot must empty them.
    for (label, name) in [
        ("C", libc::_CS_DARWIN_USER_CACHE_DIR),
        ("T", libc::_CS_DARWIN_USER_TEMP_DIR),
    ] {
        if let Some(dir) = darwin_dir(name) {
            let entries = std::fs::read_dir(&dir).map_or(0, Iterator::count);
            println!("CACHE-DIR {label} {} entries={entries}", dir.display());
        }
    }
    println!("GPU-PROBE pass");
    0
}

/// The per-user folder `confstr` names (`DARWIN_USER_CACHE_DIR`, `DARWIN_USER_TEMP_DIR`).
fn darwin_dir(name: libc::c_int) -> Option<std::path::PathBuf> {
    let mut buffer = vec![0u8; 1024];
    // SAFETY: `buffer` is writable for its whole length, which is the size passed.
    let needed = unsafe { libc::confstr(name, buffer.as_mut_ptr().cast(), buffer.len()) };
    // The length counts the NUL that ends the path.
    let path = buffer.get(..needed.checked_sub(1)?)?;
    Some(std::path::PathBuf::from(
        String::from_utf8_lossy(path).into_owned(),
    ))
}
