// Spike: Metal device and a 1024x1024 matmul compute kernel. One STAGE line per step so a
// failure names the step. Usage: gpu_probe [path/to/precompiled.metallib]
import Foundation
import Metal

func stage(_ name: String, _ ok: Bool, _ detail: String = "") {
    print("STAGE \(name) \(ok ? "ok" : "FAIL") \(detail)")
    fflush(stdout)
}

let all = MTLCopyAllDevices()
print("DEVICES count=\(all.count) names=\(all.map { $0.name })")
guard let dev = MTLCreateSystemDefaultDevice() else {
    stage("device", false, "MTLCreateSystemDefaultDevice returned nil")
    exit(3)
}
stage("device", true, "name=\(dev.name) registryID=\(dev.registryID) maxBufferLength=\(dev.maxBufferLength) unifiedMemory=\(dev.hasUnifiedMemory) lowPower=\(dev.isLowPower) headless=\(dev.isHeadless) removable=\(dev.isRemovable) location=\(dev.location.rawValue)")
var families: [String] = []
let famList: [(String, MTLGPUFamily)] = [("apple1", .apple1), ("apple5", .apple5), ("apple7", .apple7), ("apple8", .apple8), ("apple9", .apple9), ("common3", .common3), ("mac2", .mac2), ("metal3", .metal3)]
for (n, f) in famList { families.append("\(n)=\(dev.supportsFamily(f))") }
print("FAMILIES " + families.joined(separator: " "))

let n = 1024
let src = """
#include <metal_stdlib>
using namespace metal;
kernel void matmul(device const float* a [[buffer(0)]], device const float* b [[buffer(1)]],
                   device float* c [[buffer(2)]], constant uint& n [[buffer(3)]],
                   uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= n || gid.y >= n) return;
    float s = 0;
    for (uint k = 0; k < n; k++) s += a[gid.y * n + k] * b[k * n + gid.x];
    c[gid.y * n + gid.x] = s;
}
"""
guard let queue = dev.makeCommandQueue() else { stage("queue", false); exit(4) }
stage("queue", true)

var lib: MTLLibrary?
if CommandLine.arguments.count > 1 {
    do { lib = try dev.makeLibrary(URL: URL(fileURLWithPath: CommandLine.arguments[1])); stage("library-precompiled", true) }
    catch { stage("library-precompiled", false, "\(error)") }
}
if lib == nil {
    do { lib = try dev.makeLibrary(source: src, options: nil); stage("library-runtime-compile", true) }
    catch { stage("library-runtime-compile", false, "\(error)"); exit(5) }
}
guard let fn = lib?.makeFunction(name: "matmul") else { stage("function", false); exit(6) }
let pso: MTLComputePipelineState
do { pso = try dev.makeComputePipelineState(function: fn); stage("pipeline", true, "maxThreads=\(pso.maxTotalThreadsPerThreadgroup)") }
catch { stage("pipeline", false, "\(error)"); exit(7) }

var a = [Float](repeating: 0, count: n * n), b = a
for i in 0..<n { for k in 0..<n { a[i * n + k] = Float((i + k) % 4); b[i * n + k] = Float((i * k) % 3) } }
let bytes = n * n * MemoryLayout<Float>.size
guard let ba = dev.makeBuffer(bytes: a, length: bytes, options: .storageModeShared),
      let bb = dev.makeBuffer(bytes: b, length: bytes, options: .storageModeShared),
      let bc = dev.makeBuffer(length: bytes, options: .storageModeShared) else { stage("buffers", false); exit(8) }
stage("buffers", true)
var nn = UInt32(n)
guard let cb = queue.makeCommandBuffer(), let enc = cb.makeComputeCommandEncoder() else { stage("encoder", false); exit(9) }
enc.setComputePipelineState(pso)
enc.setBuffer(ba, offset: 0, index: 0); enc.setBuffer(bb, offset: 0, index: 1); enc.setBuffer(bc, offset: 0, index: 2)
enc.setBytes(&nn, length: 4, index: 3)
enc.dispatchThreads(MTLSize(width: n, height: n, depth: 1), threadsPerThreadgroup: MTLSize(width: 16, height: 16, depth: 1))
enc.endEncoding()
let t0 = Date()
cb.commit(); cb.waitUntilCompleted()
let ms = Date().timeIntervalSince(t0) * 1000
stage("dispatch", cb.status == .completed, "status=\(cb.status.rawValue) error=\(String(describing: cb.error)) ms=\(Int(ms))")
let c = bc.contents().bindMemory(to: Float.self, capacity: n * n)
var bad = 0
for _ in 0..<2000 {
    let i = Int.random(in: 0..<n), j = Int.random(in: 0..<n)
    var s: Float = 0
    for k in 0..<n { s += a[i * n + k] * b[k * n + j] }
    if c[i * n + j] != s { bad += 1 }
}
stage("result", bad == 0, "mismatches=\(bad)/2000")
print(bad == 0 && cb.status == .completed ? "GPU_PROBE_PASS" : "GPU_PROBE_FAIL")
exit(bad == 0 && cb.status == .completed ? 0 : 10)
