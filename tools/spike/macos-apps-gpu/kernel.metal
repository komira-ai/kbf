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
